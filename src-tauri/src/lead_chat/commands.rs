//! Tauri commands for the chat engine. The lead's engine is keyed by
//! `-thread_id`; chat-mode workers (phase 2) key by `session_id`.

use super::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};
use tauri::{AppHandle, Emitter, Manager, State};

pub(crate) fn lead_key(thread_id: i32) -> i64 {
    -(thread_id as i64)
}

fn log_hidden_feedback_ignored(thread_id: i32, reason: &str) {
    eprintln!(
        "[weft] post_lead_tool_result: ignored hidden feedback for thread {thread_id}: {reason}"
    );
}

fn ensure_lead_cwd(thread_id: i32) -> anyhow::Result<std::path::PathBuf> {
    let cwd = crate::paths::weft_home()?
        .join("leads")
        .join(thread_id.to_string());
    std::fs::create_dir_all(&cwd)?;
    // git-init so claude's session store (keyed by cwd) behaves like any other
    // cwd; harmless if it already exists.
    let _ = std::process::Command::new("git").env("PATH", crate::detect::tool_path())
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status();
    Ok(cwd)
}

/// Append a `--model <value>` override (issue #98) onto an engine's spawn args
/// — the ONE place this flag gets built, called from all three `EngineInner`
/// construction sites (`lead_engine`, `chat_open_worker_impl`, `worker_engine`)
/// so the shape can't drift between them. claude and codex's per-turn argv
/// builders both consume `extra_args` (`AgentAdapter::build_argv` / codex
/// app-server's own spawn, see `codex_app_server::Client::connect_session`) and
/// both accept a bare `--model <value>` flag, so no per-tool branching is
/// needed here. opencode's argv builder never reads `extra_args` at all — this
/// still pushes the flag (harmless, since nothing consumes it), and the
/// frontend keeps that honest by disabling the model field for opencode
/// (`session/engineSwitch.ts::modelSupported`) rather than silently no-op'ing
/// a value the user typed. `None`/empty is a no-op (follow the CLI's default).
fn push_model_arg(extra: &mut Vec<String>, model: Option<&str>) {
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        extra.push("--model".into());
        extra.push(m.to_string());
    }
}

/// What a (re)dispatched worker session looks like to the frontend.
#[derive(serde::Serialize, Clone)]
pub struct SessionInfo {
    pub session_id: i32,
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    /// Effective binary for the terminal resume command: the per-session
    /// pin / global alias, else the tool identity. Lets "Copy resume command"
    /// produce the actual CLI (e.g. `cc-claude`) for an aliased session.
    pub command: String,
    pub resumed: bool,
    pub native_id: Option<String>,
}

const BASE_PROMPT: &str = "You are the lead for this thread in weft — the human's main collaborator for this issue. \
Your mission: first converge WITH the human on what to build and how, then split the work into tasks. \
Do not write code. \
Operate with judgment, not a rigid script: \
- Understand the need: goal, boundaries, acceptance. Ask only as much as the issue warrants. \
- Shape an issue-level approach: architecture, cross-repo contracts, data flow, sequencing, and split rationale. Do not design each task's internal implementation — that stays the worker's job. \
- Gate the split on one plan_card. Call propose_directions only after the human approves that card (plan_decision approved, or clear agreement in chat), or when they explicitly say to skip discussion and split now. If they request changes, revise and emit a new plan_card. For trivial issues do not interrogate — emit a compact plan_card for one-click confirmation. \
Use weft_planner when it materially helps (task details, repo map / ownership, existing test cases). After workers start, coordinate on weft_bus; address workers by the numeric id in each message's `from` field. \
When proposing tasks: one task writes exactly one repo (reads are free); leave base_branch empty for the repo default and set it only for an explicit integration/release branch (ask if the base is genuinely ambiguous); pick mandate as a planning-depth hint — plan+impl when the worker should plan first, impl-only when the task is small or fully specified; prefer parallelizable tasks and put shared-contract owners first only when they block others. \
To withdraw pending tasks, call cancel_directions with a short rationale — never propose an empty directions list as a cancel. The human reviews in weft; you may re-propose after more discussion.";

/// Phase-1.5 test-case derivation (soft policy, no extra gate): when to derive,
/// the `<weft:test_cases>` sentinel shell, and edit-feedback handling. Detailed
/// workflow and quality bars live in the weft-derive-test-cases skill.
const TEST_CASES_DIRECTIVES: &str = r#"When the issue is substantial enough — or the human asks — derive acceptance-oriented TEST CASES before locking the approach; skip silently for trivial fixes, and honor an explicit ask to create or skip them. Use the weft-derive-test-cases skill for non-trivial derivation. Treat uncovered cases as gaps in the approach.

Emit or replace the document with:
<weft:test_cases>
# <title>
## <group>
- <case or sub-group>
  - <case>
</weft:test_cases>

Body is raw markdown only (no JSON, no fences). Do not restate the tree in prose. Re-emitting replaces the whole document. If you receive <weft:test_cases_updated>{"source":"user","content":"..."}</weft:test_cases_updated>, carry that content forward and re-emit only when you intentionally change it. Use language matching the user's locale for titles and cases."#;

/// The plan-card gate protocol: schema of the `<weft:plan_card>` sentinel and the
/// `<weft:plan_decision>` feedback the UI posts back. Core always-on flow.
const PLAN_CARD_DIRECTIVES: &str = r#"To present the plan for confirmation, output exactly one:
<weft:plan_card>{"title":"...","requirements":["..."],"approach":"...","split":[{"name":"...","repo":"...","reason":"..."}],"risks":["..."]}</weft:plan_card>

Field intent:
- requirements: agreed needs / acceptance
- approach: issue-level technical plan (markdown)
- split: optional coarse task preview
- risks: only unresolved risks (omit if none)

Valid JSON with normally-escaped strings: paragraph breaks inside a value are the two characters \n\n (one backslash each) — never double-escape them into literal backslash-n text. Locale matches the user. After the human acts, you may receive <weft:plan_decision>{"status":"approved"}</weft:plan_decision>; clear chat agreement counts the same. Never call propose_directions while the latest plan_card is unanswered, unless the human explicitly skipped the plan discussion."#;

/// Always-on repo action-card schema. Needed even when some repos already exist,
/// because the work may still require importing/creating/cloning another one.
const ACTION_CARD_DIRECTIVES: &str = r#"If no suitable repo exists for the work, output one:
<weft:action_card>{"title":"...","body":"...","steps":["..."],"actions":[{"id":"...","label":"...","kind":"add"|"new"|"clone"}]}</weft:action_card>

`steps` is optional. kind is add (import folder), new (create repo), or clone (remote URL). Locale matches the user. After a repo action: <weft:repo_action>{...}</weft:repo_action> with status ok/error/cancelled."#;

/// Truncation-only full-list sentinel. Appended when `<repo_state>` is truncated.
const LIST_REPOS_DIRECTIVES: &str = r#"If <repo_state> is truncated, emit on its own line:
<weft:list_repos/>
Replies arrive as <weft:list_repos_result>{...}</weft:list_repos_result>."#;

/// Always-on conversational lead prompt: role, hard gates, core sentinels, and
/// the action-card schema. Truncation-only list_repos guidance is added by
/// `lead_prompt_for` when the repo list exceeds the always-on preview.
pub fn lead_prompt() -> String {
    lead_prompt_for(false)
}

/// Build the lead system prompt. When `include_list_repos` is true, append the
/// truncation-only `<weft:list_repos/>` directive.
pub fn lead_prompt_for(include_list_repos: bool) -> String {
    let mut prompt = format!(
        "{BASE_PROMPT}\n\n{TEST_CASES_DIRECTIVES}\n\n{PLAN_CARD_DIRECTIVES}\n\n{ACTION_CARD_DIRECTIVES}"
    );
    if include_list_repos {
        prompt.push_str("\n\n");
        prompt.push_str(LIST_REPOS_DIRECTIVES);
    }
    prompt
}

/// True when `<repo_state>` is truncated and the lead may need `<weft:list_repos/>`.
pub fn needs_list_repos_directives(repo_count: usize) -> bool {
    repo_count > crate::lead_chat::repo_state::MAX_LISTED
}

/// Backward-compatible alias used by older call sites/tests.
pub fn needs_repo_sentinels(repo_count: usize) -> bool {
    needs_list_repos_directives(repo_count)
}

/// Agent-output language directive (ARCHITECTURE §4.8, layer 2). Appended to the
/// lead prompt / worker brief so prose follows the operator's UI language; code
/// and identifiers always stay English. Empty for English (the default).
pub fn lang_directive(lang: &str) -> &'static str {
    if lang == "zh" {
        "\n\n用中文撰写所有自然语言产出(计划、摘要、bus 消息、PR/commit 文案);代码、标识符与技术约定始终用英文。"
    } else {
        ""
    }
}

/// System prompt for the IM Concierge engine (M3-3). Concierge is scoped to
/// the current IM conversation — NOT a per-issue lead.
/// It never plans or writes; it only reads weft state via the `weft_global` MCP
/// and answers / triggers actions on the human's behalf. Bilingual: language
/// follows the caller's lang (defaults to zh — IM bridge fixes it that way).
pub fn concierge_prompt(lang: &str) -> String {
    let body = if lang == "zh" {
        "你是 weft 桌面端的 IM Concierge，用户从一个 IM 会话找你。weft 桌面端正在运行，真实状态都在 weft_global MCP 能力里；回答任何关于工作区、issue、待办、agent 提问的问题前，必须先用工具核实，不要凭印象作答。\n\
每条 IM 消息会带结构化 <weft:im_context>，其中包含 IM provider、当前会话、当前消息和 provider 能力。根据这些能力决定是否能创建或复用 issue 的原生 topic。\n\
当用户从 IM 创建新的 issue/task 时，必须先使用 list_workspaces 让用户选择已有 workspace；不要因为飞书会话自动新建 workspace。选择明确后使用 IM-aware 的 issue 创建能力；如果 provider 支持 issue topic，默认创建并绑定，让用户进入该 issue 的原生讨论位置。\n\
当用户希望介入已有 issue、打开 issue、继续某个 task，或把话转给某个 issue lead 时，先确保该 issue 有 provider-native topic，并引导用户进入那里。只有用户给出明确要转达给 lead 的内容时，才把 initial message 发送给 lead。\n\
普通状态查询、列表查询、待办查询不要创建 topic。无法唯一匹配 workspace 或 issue 时，先列出候选并让用户选择。\n\
不要替用户决定需要桌面确认的事（scope 拍板、批准 write trigger、合并保护分支）。不要臆造 issue/工作区/ask 的细节；找不到就说没找到。不要在不可逆动作之前自行批准权限请求，除非用户在这条消息里明确同意。\n\
回复风格：简短中文，用 markdown 列表/编号；引用 issue 时带 issue_id；引用 ask 时带 ask_id。"
    } else {
        "You are weft's IM Concierge, reached by the user through one IM conversation. weft is running on the user's desktop and authoritative state lives behind weft_global capabilities; verify with tools before answering anything about workspaces, issues, pending asks, or agent questions. Never answer from memory.\n\
Each IM message includes structured <weft:im_context> with the provider, current conversation, current message, and provider capabilities. Use those capabilities to decide whether an issue can have a provider-native topic.\n\
When the user creates a new issue/task from IM, first use list_workspaces and have the user choose an existing workspace; never create a workspace just because the user is chatting from Feishu. Once the workspace is explicit, use the IM-aware issue creation capability. If the provider supports issue topics, default to creating and binding one so the user can continue in the issue's native discussion location.\n\
When the user wants to intervene in an existing issue, open an issue, continue a task, or relay a concrete instruction to an issue lead, first ensure that issue has a provider-native topic and guide the user there. Send an initial message to the lead only when the user provided concrete text to relay.\n\
Read-only status, list, and pending-ask queries must not create topics. If a workspace or issue reference is ambiguous, list candidates and ask the user to choose.\n\
Do not decide things that require the desktop: scope approval, write-trigger approval, or protected-branch merge. Do not invent workspace, issue, or ask details. Do not pre-approve irreversible permission asks unless the user explicitly consents in this message.\n\
Style: short markdown bullets or numbered lists; mention issue_id when citing an issue and ask_id when citing an ask."
    };
    format!("{}{}", body, lang_directive(lang))
}

/// System prompt for the workspace Curator chat (`t.kind == "curator"`). It maps
/// how the workspace's repos depend on each other at runtime and through shared
/// infrastructure, and applies the human's calibrations to the graph. Read-only
/// on disk; it never modifies files or proposes issue directions.
pub fn curator_prompt() -> &'static str {
    "You are weft's repository-analysis assistant for this workspace. You map how \
the repos depend on each other at RUNTIME and through shared infrastructure — \
relationships package manifests don't capture (HTTP/REST, gRPC, message queues, \
shared databases/infra). You may READ code and config but must NEVER modify, \
create, or delete files.\n\n\
Workflow: call get_repo_map to see the repos (with ids) and the current edges. \
When the human confirms or corrects a relationship, inspect the relevant repos to \
find concrete evidence, then call calibrate_edges with action \"add\" or \"remove\" \
for exactly that edge (from/to are repo ids that must differ). Explain the \
evidence you found in plain language. Human-set edges are pinned and survive \
automatic re-analysis; removals are remembered. Make one calibrate_edges call per \
edge.\n\n\
When a repo's TIER or ROLE is wrong in get_repo_map (e.g. a frontend/local SDK \
classified as a backend service, or the wrong role within its tier), call \
set_classification with the repo id, the correct tier (frontend|backend), and the \
role/category (free text, e.g. gateway|biz|core|common|idl|support for backend; \
app|sdk|web for frontend). Classification you set is pinned and survives \
re-analysis — use it to fix what the automatic pass got wrong, not to guess.\n\n\
When the human asks you to re-analyze, regenerate the map, or analyze dependencies \
(including a one-off message sent by the Analyze / Regenerate buttons), call the \
reanalyze tool — it runs a full pass over the workspace and returns the resulting \
repo/edge counts; briefly report what it found. Do not propose issue tasks or \
write any files."
}

/// Get-or-create the lead's engine for a thread: scratch cwd, planner MCP +
/// ask bridge injections, conversational lead prompt as the system prompt.
/// Mirrors the retired PTY `plan_with_lead` wiring (spec §2).
/// Public so the IM bridge can drive the same lead engine when a飞书 thread
/// message lands on a bound issue (spec §4 / M2-3).
///
/// Concierge branch (`t.kind == "concierge"`, M3-1/-3): swap planner MCP →
/// `weft_global` MCP and the lead prompt → `concierge_prompt(lang)`. Everything
/// else (cwd, ask hook, skills) stays identical so this engine survives
/// app restarts and obeys per-task permissions the same way.
pub async fn lead_engine(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    lang: &str,
) -> anyhow::Result<EngineRef> {
    let t = repo::ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(lead_key(thread_id)) {
        return Ok(e);
    }
    let t = crate::engine_routing::prepare_initial_lead(db, &t).await?;
    let cwd = ensure_lead_cwd(thread_id)?;
    let base = app.state::<crate::BusBase>().0.clone();
    let is_concierge = t.kind == "concierge";
    let is_curator = t.kind == "curator";
    let ask = crate::bus::inject::inject_ask_hook(&base, thread_id, "lead", &t.lead_tool, &cwd);
    crate::skills::inject_for(db, t.workspace_id, &cwd).await;
    let mut extra = ask.args;
    if is_concierge {
        // Concierge is the IM-scoped global helper, not a thread participant: it
        // gets weft_global, never the per-thread bus.
        extra.extend(crate::bus::inject::inject_global(&base, &t.lead_tool, &cwd).args);
    } else if is_curator {
        // The curator chat gets its own MCP (get_repo_map + calibrate_edges) and
        // the thread bus under the LEAD identity (for streaming), but NOT the
        // planner — it calibrates the dependency graph, it doesn't propose issue
        // directions.
        extra.extend(crate::bus::inject::inject_curator(&base, thread_id, &t.lead_tool, &cwd).args);
        extra.extend(
            crate::bus::inject::inject(&base, thread_id, crate::bus::LEAD, &t.lead_tool, &cwd).args,
        );
    } else {
        // A per-issue lead gets the planner (read-only scope planning) AND the
        // thread bus under the LEAD identity, so workers can message it and it
        // can reply/broadcast. Joining the bus also lets a worker's post drive a
        // lead turn (see coordinator::run).
        extra.extend(crate::bus::inject::inject_planner(&base, thread_id, &t.lead_tool, &cwd).args);
        extra.extend(
            crate::bus::inject::inject(&base, thread_id, crate::bus::LEAD, &t.lead_tool, &cwd).args,
        );
    }
    push_model_arg(&mut extra, t.lead_model.as_deref());
    let system_prompt = if is_concierge {
        concierge_prompt(lang)
    } else if is_curator {
        let repo_state =
            crate::lead_chat::repo_state::render_repo_state(db, Some(t.workspace_id)).await?;
        format!("{}{}\n\n{}", curator_prompt(), lang_directive(lang), repo_state)
    } else {
        let repos = repo::list_repos(db, t.workspace_id).await?;
        let repo_state = crate::lead_chat::repo_state::render_repo_state_from(
            Some(t.workspace_id),
            &repos,
        );
        format!(
            "{}{}\n\n{}",
            lead_prompt_for(needs_list_repos_directives(repos.len())),
            lang_directive(lang),
            repo_state
        )
    };
    let stopped = matches!(
        repo::lead_status(db, thread_id)
            .await
            .ok()
            .flatten()
            .as_deref(),
        Some("stopped")
    );
    let mut inner = engine::EngineInner {
        thread_id,
        tool: t.lead_tool.clone(),
        command: t.lead_command.clone(),
        session_id: None,
        cwd,
        extra_args: extra,
        system_prompt,
        native_id: repo::lead_native_id(db, thread_id).await.ok().flatten(),
        pending_context_digest: None,
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, thread_id).await.unwrap_or(1) - 1,
        ask_dir: "lead".into(),
        clock: Default::default(),
        child: None,
        child_reg: None,
        child_permit: None,
        stdin: None,
        current: None,
        open_texts: std::collections::HashMap::new(),
        turn_saw_text: false,
        interrupting: false,
        generation: 0,
        reset_epoch: 0,
        pending_skill_refresh: false,
        pending_command_refresh: false,
        last_context_tokens: None,
        last_model: None,
        last_window: None,
        last_mcp_servers: vec![],
        last_tools: vec![],
        probe_seq: 0,
        probe_committed: 0,
        current_origin_tag: None,
        tool_rows: std::collections::HashMap::new(),
        stopped,
        codex_client: None,
        turn_user_row: None,
        last_assistant_uuid: None,
        rewinding: false,
        quota_failover_committing: false,
        worktree_id: None,
    };
    // Restore the last persisted meta snapshot so the Session panel is populated
    // right away after an app relaunch (not "after first message").
    engine::apply_persisted_meta(&mut inner, &t.lead_meta);
    let eng: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    Ok(state.get_or_insert(lead_key(thread_id), eng))
}

/// One inbound image attachment from the composer (pasted or picked).
#[derive(serde::Deserialize)]
pub struct ImageIn {
    pub media_type: String,
    /// base64 payload, no data-URI prefix.
    pub data: String,
}

fn to_pairs(images: Option<Vec<ImageIn>>) -> Vec<(String, String)> {
    images
        .unwrap_or_default()
        .into_iter()
        .map(|i| (i.media_type, i.data))
        .collect()
}

#[tauri::command]
pub async fn lead_send(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    text: String,
    lang: Option<String>,
    images: Option<Vec<ImageIn>>,
    files: Option<Vec<String>>,
) -> Result<(), String> {
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::send(
        &app,
        &db,
        &eng,
        &text,
        to_pairs(images),
        files.unwrap_or_default(),
        None,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn lead_interrupt(app: AppHandle, thread_id: i32) -> Result<(), String> {
    // Interrupting a curator turn must also cancel an in-flight analysis pass it
    // started (the `reanalyze` MCP tool runs in the bus backend, out of reach of the
    // engine interrupt). No-op for non-curator/idle threads. Fire it first so the
    // pass starts unwinding even if the engine interrupt blocks.
    crate::curator::cancel_analysis(thread_id);
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::interrupt(&app, &eng)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Make sure the lead engine exists + its process runs (called on console open
/// so the init event delivers slash_commands without waiting for a first send).
#[tauri::command]
pub async fn lead_ensure(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    lang: Option<String>,
) -> Result<(), String> {
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::ensure_running(&app, &db, &eng)
        .await
        .map_err(|e| e.to_string())
}

/// Stop the lead engine explicitly. Lifecycle control is separate from session
/// navigation, so copying a resume command or opening Codex never reaches here.
#[tauri::command]
pub async fn lead_stop(app: AppHandle, thread_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::stop(&app, &eng).await;
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct LeadStateInfo {
    pub state: String,
    pub queue: Vec<engine::QueuedItem>,
    pub native_id: Option<String>,
    /// Effective binary for the lead's resume command (per-thread pin / alias,
    /// else identity). Empty only when the thread row is missing.
    pub command: String,
    pub slash_commands: Vec<crate::lead_chat::proto::SlashCmd>,
    pub cwd: String,
    // —— 会话信息面板回填(常驻面板重挂不空白)——
    pub context_tokens: Option<u64>,
    pub window: Option<u64>,
    pub model: Option<String>,
    pub mcp_servers: Vec<crate::lead_chat::proto::McpServer>,
    pub tools: Vec<String>,
}

/// 由「常驻子进程是否存活」与「当前 turn 是否在跑」决定 lead engine 对外报的 state。
/// 纯函数，便于回归测试。
///
/// **busy 优先于 alive**：codex app-server 在共享连接上跑 turn，没有 per-turn 子进程，
/// 故进行中也 alive=false。若先判 alive 会把正在跑的一轮误报成 "stopped"，切页重挂时
/// loadLeadChat 便用它覆盖实时 "busy"，「处理中」占位随之消失。turn 结束/进程死时
/// busy 都会被复位（见 engine.rs on_turn_end / 死亡清理），故 busy 优先是安全的。
fn lead_state_label(alive: bool, busy: bool, stopped: bool) -> &'static str {
    if busy {
        "busy"
    } else if stopped || !alive {
        "stopped"
    } else {
        "idle"
    }
}

/// Is the lead's engine alive (can accept a send)? The exec/claude path keys off a
/// resident child; a codex app-server lead has NO per-turn child when idle but
/// stays alive while its client handle is present (a send reconnects if needed) —
/// without this a remount's `loadLeadChat` mislabels the idle lead as "stopped".
fn lead_alive(child_alive: bool, has_codex_client: bool) -> bool {
    child_alive || has_codex_client
}

#[cfg(test)]
mod tests {
    use super::{
        lead_alive, lead_prompt, lead_prompt_for, lead_state_label, needs_list_repos_directives,
        normalize_model, push_model_arg,
    };

    // issue #98 P0 (adversarial re-review of PR #139): the model override was
    // persisted, surfaced in the UI, and confirmed via SwitchOutcome/the
    // engine_switch marker — but `push_model_arg` was never actually CALLED
    // from any of the three EngineInner construction sites, so `--model` never
    // reached a spawn argv. These tests lock down the flag-building contract
    // `lead_engine` / `chat_open_worker_impl` / `worker_engine` now all share
    // (see the call sites: `push_model_arg(&mut extra, t.lead_model.as_deref())`
    // etc.) — a full AppHandle-level test through those three functions isn't
    // feasible without enabling tauri's `test` Cargo feature (not currently
    // enabled, and not something to flip on speculatively for one test), so
    // this is the closest unit-testable proof of the exact logic that broke.
    #[test]
    fn push_model_arg_appends_flag_and_value_in_order() {
        let mut extra = vec!["--settings".to_string(), "/tmp/x.json".to_string()];
        push_model_arg(&mut extra, Some("opus"));
        assert_eq!(
            extra,
            vec!["--settings", "/tmp/x.json", "--model", "opus"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn push_model_arg_is_a_noop_for_none_or_empty() {
        let mut extra = vec!["-c".to_string(), "k=v".to_string()];
        let before = extra.clone();
        push_model_arg(&mut extra, None);
        assert_eq!(extra, before, "None must not append anything");
        push_model_arg(&mut extra, Some(""));
        assert_eq!(extra, before, "empty string must not append anything either");
    }

    #[test]
    fn push_model_arg_starting_from_empty_extra_args() {
        // The common case: a thread/session with no ask-hook/MCP injection args
        // at all (can't happen in practice, but the function must not assume a
        // non-empty starting vec).
        let mut extra = vec![];
        push_model_arg(&mut extra, Some("gpt-5.5-high"));
        assert_eq!(extra, vec!["--model".to_string(), "gpt-5.5-high".to_string()]);
    }

    #[test]
    fn normalize_model_trims_and_blanks_to_none() {
        assert_eq!(normalize_model(None), None);
        assert_eq!(normalize_model(Some("".into())), None);
        assert_eq!(normalize_model(Some("   ".into())), None);
        assert_eq!(normalize_model(Some("  opus  ".into())), Some("opus".to_string()));
        assert_eq!(normalize_model(Some("gpt-5.5-high".into())), Some("gpt-5.5-high".to_string()));
    }

    /// End-to-end contract for a single switch: `normalize_model` (the switch
    /// command's own input-sanitizing step) feeding directly into
    /// `push_model_arg` (the engine-construction step) must round-trip a real
    /// override into the exact argv pair a CLI expects, with no double-trim /
    /// double-space / silent-drop surprises at the seam between the two.
    #[test]
    fn normalize_model_feeds_push_model_arg_correctly() {
        let mut extra = vec![];
        push_model_arg(&mut extra, normalize_model(Some("  opus  ".into())).as_deref());
        assert_eq!(extra, vec!["--model".to_string(), "opus".to_string()]);

        let mut extra2 = vec!["existing".to_string()];
        push_model_arg(&mut extra2, normalize_model(Some("   ".into())).as_deref());
        assert_eq!(extra2, vec!["existing".to_string()], "blank override clears to no-op");
    }

    #[test]
    fn busy_turn_reports_busy_even_without_resident_child() {
        // codex app-server 在共享连接上跑 turn，没有 per-turn 子进程，故进行中
        // alive=false。正在跑的一轮必须仍报 "busy"——否则切页重挂时 loadLeadChat
        // 会用陈旧的 "stopped" 覆盖实时态，「处理中」占位消失。（回归）
        assert_eq!(lead_state_label(false, true, true), "busy");
        assert_eq!(lead_state_label(true, true, false), "busy");
        assert_eq!(lead_state_label(true, false, false), "idle");
        assert_eq!(lead_state_label(true, false, true), "stopped");
        assert_eq!(lead_state_label(false, false, false), "stopped");
    }

    #[test]
    fn idle_appserver_lead_is_alive_without_a_child() {
        // An idle codex app-server lead has no per-turn child but a live client
        // handle → alive, so it reports "idle" not "stopped" on remount.
        assert!(lead_alive(false, true)); // no child, app-server client present
        assert!(lead_alive(true, false)); // resident child (exec/claude)
        assert!(!lead_alive(false, false)); // neither → genuinely down
        assert_eq!(lead_state_label(lead_alive(false, true), false, false), "idle");
    }

    #[test]
    fn lead_prompt_is_policy_not_fixed_sequence() {
        let prompt = lead_prompt();
        assert!(prompt.contains("converge WITH the human"));
        assert!(prompt.contains("Operate with judgment, not a rigid script"));
        assert!(prompt.contains("Use weft_planner when it materially helps"));
        assert!(!prompt.contains("Start by greeting"));
        assert!(!prompt.contains("call get_task"));
        // Action-card schema is always-on so a nonempty workspace can still
        // import/create/clone another repo; list_repos is truncation-only.
        assert!(prompt.contains("<weft:action_card>"));
        assert!(!prompt.contains("<weft:list_repos/>"));
        assert!(lead_prompt_for(true).contains("<weft:list_repos/>"));
        assert!(!needs_list_repos_directives(0));
        assert!(needs_list_repos_directives(crate::lead_chat::repo_state::MAX_LISTED + 1));
        assert!(!needs_list_repos_directives(1));
    }

    /// The discuss-first gate: the split happens only after the human approves
    /// the plan card (or explicitly skips). Judgment replaces rigid red-team
    /// scripts; the old "split as soon as scope is clear" bar stays gone.
    #[test]
    fn lead_prompt_gates_split_on_plan_approval() {
        let prompt = lead_prompt();
        assert!(prompt.contains("<weft:plan_card>"));
        assert!(prompt.contains("<weft:plan_decision>"));
        assert!(prompt.contains("Call propose_directions only after the human approves"));
        assert!(prompt.contains("skip"));
        // Adaptive floor: trivial issues get a compact card, not an interrogation.
        assert!(prompt.contains("For trivial issues do not interrogate"));
        // Parser persists plan cards verbatim, so the prompt must mandate
        // normally-escaped JSON paragraph breaks (not double-escaped \\n).
        assert!(prompt.contains("normally-escaped"));
        assert!(prompt.contains("never double-escape"));
        // The eager-split bar and the question-rationing rule must be gone.
        assert!(!prompt.contains("clear enough for workers to start"));
        assert!(!prompt.contains("Ask clarifying questions only when"));
        // Workers still own their direction's implementation.
        assert!(prompt.contains("that stays the worker's job"));
    }

    /// Phase 1.5: the test-case derivation is a soft policy — adaptive, no new
    /// gate — with a raw-markdown sentinel, edit-feedback loop, and skill pointer.
    #[test]
    fn lead_prompt_derives_test_cases_before_shaping() {
        let prompt = lead_prompt();
        assert!(prompt.contains("<weft:test_cases>"));
        assert!(prompt.contains("before locking the approach"));
        assert!(prompt.contains("skip silently for trivial fixes"));
        assert!(prompt.contains("raw markdown only"));
        assert!(prompt.contains("<weft:test_cases_updated>"));
        // Approach must still be checked against the cases; detailed quality
        // bars live in the skill rather than always-on prompt prose.
        assert!(prompt.contains("Treat uncovered cases as gaps in the approach"));
        assert!(prompt.contains("weft-derive-test-cases"));
        assert!(!prompt.contains("every leaf decidable"));
        assert!(!prompt.contains("user language only"));
    }

    /// plan_decision feedback gets its own sentinel tag; everything else keeps
    /// the historical repo_action tag (repo-onboarding flows depend on it).
    #[test]
    fn hidden_feedback_text_routes_tag_by_tool() {
        let plan = serde_json::json!({"tool": "plan_decision", "status": "approved"});
        let text = super::hidden_feedback_text(&plan).unwrap();
        assert!(text.starts_with("<weft:plan_decision>"));
        assert!(text.ends_with("</weft:plan_decision>"));
        assert!(text.contains("\"approved\""));

        let repo = serde_json::json!({"tool": "repo_action", "status": "ok"});
        let text = super::hidden_feedback_text(&repo).unwrap();
        assert!(text.starts_with("<weft:repo_action>"));

        // No tool field → historical default.
        let bare = serde_json::json!({"status": "ok"});
        let text = super::hidden_feedback_text(&bare).unwrap();
        assert!(text.starts_with("<weft:repo_action>"));
    }

    #[test]
    fn lead_prompt_action_card_schema_includes_optional_steps() {
        let always = lead_prompt();
        assert!(always.contains("\"steps\""));
        assert!(always.contains("`steps` is optional"));
        assert!(always.contains("<weft:repo_action>"));
        assert!(!always.contains("<weft:list_repos/>"));
        let with_list = lead_prompt_for(true);
        assert!(with_list.contains("<weft:list_repos/>"));
    }

    #[test]
    fn stale_cleanup_skips_only_busy_sessions() {
        use std::collections::HashSet;
        // Session 1 busy; lead (None) + session 2 idle → clean lead and [2], not 1.
        let busy: HashSet<Option<i32>> = [Some(1)].into_iter().collect();
        let (clean_lead, sessions) = super::stale_cleanup_targets(&busy, &[1, 2]);
        assert!(clean_lead);
        assert_eq!(sessions, vec![2]);
        // Lead (None) busy → don't clean the lead group; idle sessions still cleaned.
        let busy2: HashSet<Option<i32>> = [None].into_iter().collect();
        let (clean_lead2, sessions2) = super::stale_cleanup_targets(&busy2, &[1, 2]);
        assert!(!clean_lead2);
        assert_eq!(sessions2, vec![1, 2]);
        // Nothing busy → clean every group.
        let (cl3, s3) = super::stale_cleanup_targets(&HashSet::new(), &[1, 2]);
        assert!(cl3);
        assert_eq!(s3, vec![1, 2]);
    }

    #[test]
    fn concierge_prompt_is_provider_aware_not_feishu_scripted() {
        let prompt = super::concierge_prompt("zh");
        assert!(prompt.contains("IM provider"));
        assert!(prompt.contains("provider-native"));
        assert!(prompt.contains("创建并绑定"));
        assert!(!prompt.contains("feishu_chat_id"));
        assert!(!prompt.contains("ensure_issue_topic"));
    }

    /// Guard against field-name drift: serde must emit `queue` (array) not
    /// `queued` (count) so the TS side's `LeadStateInfo.queue` hydrates correctly.
    #[test]
    fn lead_state_info_serialises_queue_key_not_queued() {
        use crate::lead_chat::engine::QueuedItem;
        let info = super::LeadStateInfo {
            state: "idle".into(),
            queue: vec![QueuedItem { id: 1, text: "hello".into(), images: 0, files: 0, has_attachments: false }],
            native_id: None,
            command: String::new(),
            slash_commands: vec![],
            cwd: "/tmp".into(),
            context_tokens: None,
            window: None,
            model: None,
            mcp_servers: vec![],
            tools: vec![],
        };
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("queue").is_some(), "must serialize as `queue`");
        assert!(v.get("queued").is_none(), "must NOT serialize as `queued`");
        assert!(v["queue"].is_array(), "`queue` must be an array");
        assert_eq!(v["queue"][0]["id"], 1);
    }
}

#[tauri::command]
pub async fn lead_state(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<LeadStateInfo, String> {
    let eng = app.state::<LeadChatState>().get(lead_key(thread_id));
    match eng {
        None => {
            let t = repo::get_thread(&db, thread_id).await.ok().flatten();
            // No live engine yet (e.g. right after an app relaunch): serve the
            // persisted meta snapshot so the Session panel isn't blank until the
            // first message spins the engine up.
            let mut snap = engine::PersistedMeta::default();
            if let Some(t) = &t {
                if !t.lead_meta.is_empty() {
                    if let Ok(m) = serde_json::from_str::<engine::PersistedMeta>(&t.lead_meta) {
                        snap = m;
                    }
                }
            }
            Ok(LeadStateInfo {
                state: "stopped".into(),
                queue: vec![],
                native_id: repo::lead_native_id(&db, thread_id).await.ok().flatten(),
                command: t
                    .map(|t| {
                        crate::tool_command::effective(t.lead_command.as_deref(), &t.lead_tool)
                    })
                    .unwrap_or_default(),
                slash_commands: vec![],
                cwd: crate::paths::weft_home()
                    .map(|h| {
                        h.join("leads")
                            .join(thread_id.to_string())
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_default(),
                context_tokens: snap.context_tokens,
                window: snap.window,
                model: snap.model,
                mcp_servers: snap.mcp_servers,
                tools: snap.tools,
            })
        }
        Some(e) => {
            let mut i = e.lock().await;
            let child_alive = i
                .child
                .as_mut()
                .map(|c| c.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            // codex app-server leads are childless when idle but alive via the client.
            let alive = lead_alive(child_alive, i.codex_client.is_some());
            let command = crate::tool_command::effective(i.command.as_deref(), &i.tool);
            Ok(LeadStateInfo {
                state: lead_state_label(alive, i.turn.busy, i.stopped).into(),
                queue: engine::queue_items(&i.turn),
                native_id: i.native_id.clone(),
                command,
                slash_commands: i.slash_commands.clone(),
                cwd: i.cwd.to_string_lossy().into_owned(),
                context_tokens: i.last_context_tokens,
                window: i.last_window,
                model: i.last_model.clone(),
                mcp_servers: i.last_mcp_servers.clone(),
                tools: i.last_tools.clone(),
            })
        }
    }
}

/// 会话信息面板的带外 meta。codex/opencode 补 model / window / MCP server;claude 的
/// model/window/MCP 走事件流(init/usage),这里只补它的 skill（`gather_claude` 现扫
/// cwd 的 skill 目录,其余字段留 None,`mergeSnapshot` 的 `?? prev` 保住事件填的富 meta）。
#[tauri::command]
pub async fn lead_session_meta(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<Option<crate::session_meta::SessionMetaSnapshot>, String> {
    let Some(t) = repo::get_thread(&db, thread_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let cwd = ensure_lead_cwd(thread_id).map_err(|e| e.to_string())?;
    let native = repo::lead_native_id(&db, thread_id).await.ok().flatten();
    let command = crate::tool_command::effective(t.lead_command.as_deref(), &t.lead_tool);
    // Ticket BEFORE gathering: a slow probe overlapping a fresher one must not
    // roll usage back when it finally lands (see absorb_probe_meta).
    let ticket = engine::take_probe_ticket(&app, thread_id, None).await;
    let snap = crate::session_meta::gather(
        &t.lead_tool,
        &cwd.to_string_lossy(),
        native.as_deref(),
        &command,
    )
    .await;
    // Probe results feed the engine cache + persisted snapshot: codex/opencode
    // model/window/MCP only exist here, never in engine events.
    engine::absorb_probe_meta(&app, &db, thread_id, None, ticket, &snap).await;
    Ok(Some(snap))
}

/// Discover the slash commands a session's CLI actually supports — never
/// hardcoded for tools whose CLI exposes the list. claude: the live
/// `initialize` list the engine already holds; opencode: GET /command off a
/// lazily-started `opencode serve`, keyed by the session's project cwd; codex:
/// the TUI's built-in enum mirrored locally (codex's app-server has no slash
/// surface, see `codex_slash`) merged with dynamic skills from `skills/list`.
/// `session_id` selects a worker; `thread_id` selects the lead.
#[tauri::command]
pub async fn discover_slash(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: Option<i32>,
    session_id: Option<i32>,
) -> Result<Vec<crate::lead_chat::proto::SlashCmd>, String> {
    let state = app.state::<LeadChatState>();
    if let Some(sid) = session_id {
        let Some(sess) = repo::get_session(&db, sid)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(vec![]);
        };
        return Ok(match sess.tool.as_str() {
            "opencode" => merge_local_skill_commands(
                crate::opencode::discover_commands(
                    &sess.cwd,
                    &crate::tool_command::effective(sess.command.as_deref(), &sess.tool),
                )
                .await,
                std::path::Path::new(&sess.cwd),
            ),
            "claude" => {
                let eng = match state.get(sid as i64) {
                    Some(eng) => eng,
                    None => worker_engine(&app, &db, sid)
                        .await
                        .map_err(|e| e.to_string())?,
                };
                engine::ensure_running(&app, &db, &eng)
                    .await
                    .map_err(|e| e.to_string())?;
                merge_local_skill_commands(
                    wait_for_slash_commands(&eng).await,
                    std::path::Path::new(&sess.cwd),
                )
            }
            "codex" => {
                crate::codex_slash::discover_commands_for_cwd(std::path::Path::new(&sess.cwd)).await
            }
            _ => vec![],
        });
    }
    // Lead console: claude carries its own initialize list on the engine;
    // codex and opencode use the same fallback discovery as workers so the
    // composer still gets a palette before the lead process has emitted init.
    if let Some(tid) = thread_id {
        if let Some(eng) = state.get(lead_key(tid)) {
            let (live, tool, cwd, command) = {
                let inner = eng.lock().await;
                (
                    inner.slash_commands.clone(),
                    inner.tool.clone(),
                    inner.cwd.clone(),
                    crate::tool_command::effective(inner.command.as_deref(), &inner.tool),
                )
            };
            let discovered = match tool.as_str() {
                "claude" if live.is_empty() => {
                    engine::ensure_running(&app, &db, &eng)
                        .await
                        .map_err(|e| e.to_string())?;
                    merge_local_skill_commands(wait_for_slash_commands(&eng).await, &cwd)
                }
                "claude" => merge_local_skill_commands(live, &cwd),
                "opencode" => {
                    let cmds =
                        crate::opencode::discover_commands(&cwd.to_string_lossy(), &command).await;
                    let cmds = if cmds.is_empty() { live } else { cmds };
                    merge_local_skill_commands(cmds, &cwd)
                }
                "codex" => {
                    let cmds = crate::codex_slash::discover_commands_for_cwd(&cwd).await;
                    if cmds.is_empty() {
                        live
                    } else {
                        cmds
                    }
                }
                _ => live,
            };
            if !discovered.is_empty() {
                eng.lock().await.slash_commands = discovered.clone();
            }
            return Ok(discovered);
        } else if let Ok(Some(t)) = repo::get_thread(&db, tid).await {
            // Lead engine not spawned yet — composer still wants a palette.
            return Ok(match t.lead_tool.as_str() {
                "opencode" => {
                    let cwd = ensure_lead_cwd(tid).map_err(|e| e.to_string())?;
                    crate::skills::inject_for(&db, t.workspace_id, &cwd).await;
                    let command =
                        crate::tool_command::effective(t.lead_command.as_deref(), &t.lead_tool);
                    merge_local_skill_commands(
                        crate::opencode::discover_commands(&cwd.to_string_lossy(), &command).await,
                        &cwd,
                    )
                }
                "codex" => {
                    let cwd = ensure_lead_cwd(tid).map_err(|e| e.to_string())?;
                    crate::skills::inject_for(&db, t.workspace_id, &cwd).await;
                    crate::codex_slash::discover_commands_for_cwd(&cwd).await
                }
                _ => vec![],
            });
        }
    }
    Ok(vec![])
}

fn merge_local_skill_commands(
    mut commands: Vec<crate::lead_chat::proto::SlashCmd>,
    cwd: &std::path::Path,
) -> Vec<crate::lead_chat::proto::SlashCmd> {
    let mut seen: std::collections::HashSet<String> =
        commands.iter().map(|c| c.name.clone()).collect();
    for c in crate::codex_slash::local_skill_commands_for_cwd(cwd) {
        if seen.insert(c.name.clone()) {
            commands.push(c);
        }
    }
    commands
}

async fn wait_for_slash_commands(eng: &EngineRef) -> Vec<crate::lead_chat::proto::SlashCmd> {
    for _ in 0..20 {
        let cmds = eng.lock().await.slash_commands.clone();
        if !cmds.is_empty() {
            return cmds;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    vec![]
}

/// A streaming row is only legitimately live while ITS OWN engine is busy, so the
/// `None` (lead, sessionless) group and each `Some(session)` group is cleanable
/// unless that exact engine is busy. Returns (clean the lead group?, the session
/// ids to clean). Gating per `(thread, session)` instead of one issue-wide busy
/// flag stops a busy session from leaving another idle session's stale row stuck
/// in `streaming` (a forever-"typing" assistant) until the next all-idle reload.
fn stale_cleanup_targets(
    busy: &std::collections::HashSet<Option<i32>>,
    sessions: &[i32],
) -> (bool, Vec<i32>) {
    let clean_lead = !busy.contains(&None);
    let clean_sessions = sessions
        .iter()
        .copied()
        .filter(|s| !busy.contains(&Some(*s)))
        .collect();
    (clean_lead, clean_sessions)
}

#[tauri::command]
pub async fn list_lead_messages(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<Vec<crate::store::entities::lead_message::Model>, String> {
    let engines: Vec<EngineRef> = {
        let state = app.state::<LeadChatState>();
        state.0.iter().map(|r| r.value().clone()).collect()
    };
    let mut busy: std::collections::HashSet<Option<i32>> = std::collections::HashSet::new();
    for eng in engines {
        let inner = eng.lock().await;
        if inner.thread_id == thread_id && inner.turn.busy {
            busy.insert(inner.session_id);
        }
    }
    let sessions = repo::sessions_for_thread(&db, thread_id)
        .await
        .map_err(|e| e.to_string())?;
    let session_ids: Vec<i32> = sessions.iter().map(|s| s.id).collect();
    let (clean_lead, clean_sessions) = stale_cleanup_targets(&busy, &session_ids);
    if clean_lead {
        repo::mark_incomplete_turns_interrupted(&db, thread_id, None)
            .await
            .map_err(|e| e.to_string())?;
        // Un-sent queued rows from a dead session surface as resendable errors, not stuck/invisible.
        repo::fail_queued(&db, thread_id, None)
            .await
            .map_err(|e| e.to_string())?;
    }
    for sid in clean_sessions {
        repo::mark_incomplete_turns_interrupted(&db, thread_id, Some(sid))
            .await
            .map_err(|e| e.to_string())?;
        // Un-sent queued rows from a dead session surface as resendable errors, not stuck/invisible.
        repo::fail_queued(&db, thread_id, Some(sid))
            .await
            .map_err(|e| e.to_string())?;
    }
    repo::list_lead_messages(&db, thread_id)
        .await
        .map_err(|e| e.to_string())
}

/// One live worker engine, advertised to the frontend so a backend-headless
/// worker (boot recovery, or one still alive after a frontend reload) gets a
/// status dot + auto-verify instead of running invisibly. `info` mirrors what
/// `chat_open_worker` returns, so the frontend can adopt it WITHOUT a
/// re-attach/ensure_running call (which would restart a stopped worker).
#[derive(serde::Serialize)]
pub struct LiveWorkerSlot {
    pub info: SessionInfo,
    pub direction_id: i32,
    pub repo_id: i32,
    /// The worker's OWN thread (from EngineInner.thread_id) — adoption must not
    /// assume the active thread.
    pub thread_id: i32,
    pub busy: bool,
    pub queue: Vec<engine::QueuedItem>,
}

/// A snapshot of one live worker engine taken under its lock. Pure input to
/// `build_worker_slots`, so the busy-filter + DB assembly is testable without a
/// running Tauri app / `LeadChatState`.
struct WorkerSnapshot {
    session_id: i32,
    thread_id: i32,
    busy: bool,
    queue: Vec<engine::QueuedItem>,
}

/// Keep only the actually-running snapshots (`busy == true`) and assemble each
/// into a full slot from the DB. `turn.busy` is the reliable "running, not
/// stopped" signal: `stop`/`stop_quiet` reset `turn` to default (busy=false) and
/// it is tool-independent (true during a codex per-turn run, false between
/// turns). Best-effort: a snapshot whose session/worktree row is missing is
/// skipped, not fatal.
async fn build_worker_slots(db: &Db, snaps: Vec<WorkerSnapshot>) -> Vec<LiveWorkerSlot> {
    let mut out = Vec::new();
    for s in snaps {
        if !s.busy {
            continue;
        }
        let Ok(Some(sess)) = repo::get_session(db, s.session_id).await else {
            continue;
        };
        let Ok(Some(wt)) = repo::worktree_for(db, sess.direction_id, sess.repo_id).await else {
            continue;
        };
        let command = crate::tool_command::effective(sess.command.as_deref(), &sess.tool);
        out.push(LiveWorkerSlot {
            info: SessionInfo {
                session_id: sess.id,
                repo: wt.path.clone(),
                worktree: wt.path,
                branch: wt.branch,
                tool: sess.tool,
                command,
                resumed: sess.native_session_id.is_some(),
                native_id: sess.native_session_id,
            },
            direction_id: sess.direction_id,
            repo_id: sess.repo_id,
            thread_id: s.thread_id,
            busy: s.busy,
            queue: s.queue,
        });
    }
    out
}

/// Live worker engines the frontend should mirror into its session map. Snapshots
/// `LeadChatState` worker entries (positive keys = session ids; negative keys are
/// leads), then `build_worker_slots` keeps only the running ones. Read-only: it
/// never starts or attaches an engine, so it cannot restart a stopped worker.
#[tauri::command]
pub async fn list_live_worker_slots(
    app: AppHandle,
    db: State<'_, Db>,
) -> Result<Vec<LiveWorkerSlot>, String> {
    // Clone the worker EngineRefs out of the registry; each DashMap ref is dropped
    // by the collect, so nothing is held across the tokio engine lock below.
    // Positive keys are worker session ids; negative keys are leads.
    let engines: Vec<EngineRef> = {
        let state = app.state::<LeadChatState>();
        state
            .0
            .iter()
            .filter(|r| *r.key() > 0)
            .map(|r| r.value().clone())
            .collect()
    };
    let mut snaps = Vec::new();
    for eng in engines {
        let (session_id, thread_id, busy, queue) = {
            let inner = eng.lock().await;
            (inner.session_id, inner.thread_id, inner.turn.busy, engine::queue_items(&inner.turn))
        };
        if let Some(sid) = session_id {
            snaps.push(WorkerSnapshot { session_id: sid, thread_id, busy, queue });
        }
    }
    Ok(build_worker_slots(&db, snaps).await)
}

/// Backend-authoritative auto-verify gate. Given a worker session that just ended
/// a turn, return its direction id IFF the direction is in an implementation phase
/// (working/review) — read FRESH from the DB so a phase that changed mid-turn (e.g.
/// planning→working via the bus set_task_status) is reflected, not a stale snapshot.
/// The frontend calls this on a worker idle turn push and runs verify_direction when
/// it returns Some; this replaces the racy frontend busy→idle / phase-cache effect.
#[tauri::command]
pub async fn auto_verify_check(
    db: State<'_, Db>,
    session_id: i32,
) -> Result<Option<i32>, String> {
    let Some(sess) = repo::get_session(&db, session_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let Some(dir) = repo::get_direction(&db, sess.direction_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Ok((dir.status == "working" || dir.status == "review").then_some(sess.direction_id))
}

// ───────────────────── chat-mode workers ─────────────────────
//
// Every worker (claude/codex/opencode) runs on the engine: a weft-owned chat
// timeline in the worker conversation surface, with per-tool wire dialects
// (engine::per_turn).
// Native-session re-entry is non-destructive: Codex opens its app link; other
// tools receive a terminal resume command for the native id.

/// Spawn (or resume) a chat-mode worker for a (direction, repo) slot: worktree
/// cwd, thread-bus MCP + ask bridge, the assembled brief as the first user
/// message of a weft-owned conversation.
#[tauri::command]
pub async fn chat_open_worker(
    app: AppHandle,
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
    lang: Option<String>,
) -> Result<SessionInfo, String> {
    chat_open_worker_impl(
        &app,
        &db,
        direction_id,
        repo_id,
        lang.as_deref().unwrap_or("en"),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Persist the automatic decision made when a sessionless worker first opens.
/// The audit marker is required even when the current tool already matches the
/// stored direction: an earlier planner marker can describe a stale route.
async fn reconcile_initial_worker_route(
    db: &Db,
    direction: &mut crate::store::entities::direction::Model,
    prior_session_id: Option<i32>,
    route: &crate::engine_routing::RouteDecision,
) -> anyhow::Result<()> {
    let selected = route
        .selected()
        .ok_or_else(|| anyhow::anyhow!("unblocked worker route had no selected engine"))?;
    if selected.as_str() != direction.tool.as_str() {
        repo::refresh_unpinned_direction_route(
            db,
            direction.id,
            prior_session_id,
            selected.as_str(),
        )
        .await?;
        direction.tool = selected.as_str().to_string();
    }
    crate::engine_routing::record_decision(
        db,
        direction.thread_id,
        None,
        Some(direction.id),
        "worker_start",
        route,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod initial_worker_route_tests {
    use super::reconcile_initial_worker_route;
    use crate::engine_quota::QuotaStatus;
    use crate::engine_routing::{EngineId, RouteDecision, RouteReason, RoutingHint, RoutingSource};
    use crate::store::{repo, Db};

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn initial_worker_route_records_the_selected_tool_when_it_is_unchanged() {
        let db = mem().await;
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, workspace.id, "repo", "/tmp/repo", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let mut direction = repo::create_direction(
            &db,
            thread.id,
            "alpha",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let stale = serde_json::json!({
            "tool": "claude",
            "source": "automatic",
            "operation": "planner_confirm",
            "direction_id": direction.id,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "engine_route",
            &stale,
            "complete",
        )
        .await
        .unwrap();
        let route = RouteDecision {
            tool: Some(EngineId::Codex),
            source: RoutingSource::Automatic,
            reason: RouteReason::NormalPreference,
            hint: RoutingHint::Normal,
            quota: Some(QuotaStatus::Ok),
            blocked: false,
        };

        reconcile_initial_worker_route(&db, &mut direction, None, &route)
            .await
            .unwrap();

        let marker = repo::list_lead_messages(&db, thread.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|message| message.kind == "engine_route" && message.session_id.is_none())
            .last()
            .unwrap();
        let content: serde_json::Value = serde_json::from_str(&marker.content).unwrap();
        assert_eq!(content["operation"], "worker_start");
        assert_eq!(content["tool"], "codex");
        assert_eq!(content["direction_id"], direction.id);
    }
}

pub(crate) async fn chat_open_worker_impl(
    app: &AppHandle,
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    lang: &str,
) -> anyhow::Result<SessionInfo> {
    let wt = repo::worktree_for(db, direction_id, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no materialized worktree for that direction+repo"))?;
    repo::ensure_repo_workspace_accepts_writes(db, repo_id).await?;
    let cwd = std::path::PathBuf::from(&wt.path);
    // A worktree row can outlive its directory (reclaimed via the Done-card
    // delete, or removed out of band). Never spawn a worker in a missing cwd —
    // this is the chokepoint every driver (card, revive, redispatch, review)
    // funnels through, so the guard covers them all.
    if !cwd.exists() {
        anyhow::bail!("worktree directory no longer exists for that direction+repo");
    }

    // A planner's final manual pin and a worker's first engine registration
    // own the same initial route. Hold this through the first start/send so a
    // stale no-native engine cannot appear between the planner's liveness check
    // and its durable route transaction.
    let _initial_route_guard = engine::initial_worker_route_gate(direction_id)
        .lock_owned()
        .await;
    let mut dir = repo::ensure_direction_workspace_accepts_writes(db, direction_id).await?;

    // An unpinned direction that has not yet established a native conversation
    // is still an initial route, not a running participant. Re-resolve it so a
    // delayed open cannot launch an engine that the current automatic policy
    // excludes. Once a worker has a native conversation or a live engine, keep
    // its selected identity: automatic routing is not a license to migrate an
    // existing session on open.
    let state = app.state::<LeadChatState>();
    let prior = repo::latest_session_for(db, direction_id, repo_id).await?;
    let has_live_prior = prior
        .as_ref()
        .is_some_and(|session| state.worker_is_running(session.id));
    if let Some(session) = prior
        .as_ref()
        .filter(|session| session.native_session_id.is_none() && !has_live_prior)
    {
        if let Some(stale) = state.get(session.id as i64) {
            if let Some(stale) = state.remove_if_same(session.id as i64, &stale) {
                let _ = engine::teardown_for_switch(app, &stale).await;
            }
        }
    }
    let can_refresh_initial_route = !dir.engine_pinned
        && !has_live_prior
        && prior
            .as_ref()
            .map_or(true, |session| session.native_session_id.is_none());
    let route_hint = if can_refresh_initial_route {
        crate::planner::direction_routing_hint(db, dir.thread_id, direction_id).await?
    } else {
        crate::engine_routing::RoutingHint::Normal
    };
    let legacy_tool = crate::tools::default_tool(db).await;
    let route = crate::engine_routing::resolve_for_db(
        db,
        if can_refresh_initial_route {
            None
        } else {
            Some(&dir.tool)
        },
        &legacy_tool,
        route_hint,
    )
    .await;
    if route.blocked {
        anyhow::bail!("engine_route_blocked:{}", route.reason_code());
    }
    if can_refresh_initial_route {
        reconcile_initial_worker_route(
            db,
            &mut dir,
            prior.as_ref().map(|session| session.id),
            &route,
        )
        .await?;
    }

    // Resume an earlier conversation when this slot already captured one.
    let prior = repo::latest_session_for(db, direction_id, repo_id).await?;
    let native = prior.as_ref().and_then(|s| s.native_session_id.clone());
    let resumed = native.is_some();
    let sess = match prior {
        Some(s) if s.native_session_id.is_some() => s,
        _ => repo::create_session_for_current_direction(db, direction_id, repo_id, &wt.path)
            .await?,
    };
    let session_tool = sess.tool.clone();
    crate::engine_routing::mirror_direction_route(db, dir.thread_id, direction_id, sess.id).await;

    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &session_tool,
        &cwd,
    );
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &session_tool,
        &cwd,
    );
    if let Ok(Some(th)) = repo::get_thread(db, dir.thread_id).await {
        crate::skills::inject_for(db, th.workspace_id, &cwd).await;
    }
    let mut extra = ask.args;
    extra.extend(inj.args);
    push_model_arg(&mut extra, sess.model.as_deref());

    let key = sess.id as i64;
    repo::ensure_thread_workspace_accepts_writes(db, dir.thread_id).await?;
    let eng = match state.get(key) {
        Some(e) => e,
        None => {
            let mut inner = engine::EngineInner {
                thread_id: dir.thread_id,
                tool: session_tool.clone(),
                command: sess.command.clone(),
                session_id: Some(sess.id),
                cwd,
                extra_args: extra,
                system_prompt: String::new(),
                native_id: native.clone(),
                pending_context_digest: None,
                slash_commands: vec![],
                turn: Default::default(),
                turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
                ask_dir: direction_id.to_string(),
                clock: Default::default(),
                child: None,
                child_reg: None,
                child_permit: None,
                stdin: None,
                current: None,
                open_texts: std::collections::HashMap::new(),
                turn_saw_text: false,
                interrupting: false,
                generation: 0,
                reset_epoch: 0,
                pending_skill_refresh: false,
                pending_command_refresh: false,
                last_context_tokens: None,
                last_model: None,
                last_window: None,
                last_mcp_servers: vec![],
                last_tools: vec![],
                probe_seq: 0,
                probe_committed: 0,
                current_origin_tag: None,
                tool_rows: std::collections::HashMap::new(),
                stopped: sess.status == "stopped",
                codex_client: None,
                turn_user_row: None,
                last_assistant_uuid: None,
                rewinding: false,
                quota_failover_committing: false,
                worktree_id: Some(wt.id),
            };
            // Restore the last persisted meta snapshot so the Session panel is
            // populated right away after an app relaunch (not "after first message").
            engine::apply_persisted_meta(&mut inner, &sess.meta);
            let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
            state.get_or_insert(key, e)
        }
    };
    if let Err(err) = engine::ensure_running(app, db, &eng).await {
        if let Some(stale) = state.remove_if_same(key, &eng) {
            let _ = engine::teardown_for_switch(app, &stale).await;
        }
        return Err(err);
    }

    // A fresh conversation starts with a user-shaped task request, followed by
    // the structured Weft brief as context.
    if !resumed {
        let mut brief = crate::brief::assemble(db, direction_id)
            .await
            .unwrap_or_default();
        if !brief.trim().is_empty() {
            brief.push_str(lang_directive(lang));
            if let Err(err) = engine::send(app, db, &eng, &brief, vec![], vec![], None).await {
                if let Some(stale) = state.remove_if_same(key, &eng) {
                    let _ = engine::teardown_for_switch(app, &stale).await;
                }
                return Err(err);
            }
        }
    }
    // Dispatch enters the mandate's first phase: plan+impl workers start by
    // planning their direction (the brief says so); impl-only build right away.
    // Resume keeps whatever status the agent last reported.
    if !resumed {
        let phase = if repo::normalize_mandate(&dir.mandate) == "impl-only" {
            "working"
        } else {
            "planning"
        };
        let _ = repo::set_direction_status(db, direction_id, phase).await;
    }

    let command = crate::tool_command::effective(sess.command.as_deref(), &session_tool);
    Ok(SessionInfo {
        session_id: sess.id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: session_tool,
        command,
        resumed,
        native_id: native,
    })
}

/// Get-or-rebuild a worker's engine from its session row — so a chat worker
/// survives app restarts the same way the lead does: sending resumes it.
async fn worker_engine(app: &AppHandle, db: &Db, session_id: i32) -> anyhow::Result<EngineRef> {
    let sess = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let dir = repo::ensure_direction_workspace_accepts_writes(db, sess.direction_id).await?;
    repo::ensure_repo_workspace_accepts_writes(db, sess.repo_id).await?;
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(session_id as i64) {
        return Ok(e);
    }
    let cwd = std::path::PathBuf::from(&sess.cwd);
    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(
        &base,
        dir.thread_id,
        &sess.direction_id.to_string(),
        &sess.tool,
        &cwd,
    );
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        dir.thread_id,
        &sess.direction_id.to_string(),
        &sess.tool,
        &cwd,
    );
    if let Ok(Some(th)) = repo::get_thread(db, dir.thread_id).await {
        crate::skills::inject_for(db, th.workspace_id, &cwd).await;
    }
    let mut extra = ask.args;
    extra.extend(inj.args);
    push_model_arg(&mut extra, sess.model.as_deref());
    let mut inner = engine::EngineInner {
        thread_id: dir.thread_id,
        tool: sess.tool.clone(),
        command: sess.command.clone(),
        session_id: Some(sess.id),
        cwd,
        extra_args: extra,
        system_prompt: String::new(),
        native_id: sess.native_session_id.clone(),
        pending_context_digest: None,
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
        ask_dir: sess.direction_id.to_string(),
        clock: Default::default(),
        child: None,
        child_reg: None,
        child_permit: None,
        stdin: None,
        current: None,
        open_texts: std::collections::HashMap::new(),
        turn_saw_text: false,
        interrupting: false,
        generation: 0,
        reset_epoch: 0,
        pending_skill_refresh: false,
        pending_command_refresh: false,
        last_context_tokens: None,
        last_model: None,
        last_window: None,
        last_mcp_servers: vec![],
        last_tools: vec![],
        probe_seq: 0,
        probe_committed: 0,
        current_origin_tag: None,
        tool_rows: std::collections::HashMap::new(),
        stopped: sess.status == "stopped",
        codex_client: None,
        turn_user_row: None,
        last_assistant_uuid: None,
        rewinding: false,
        quota_failover_committing: false,
        // One cheap lookup at engine build so send's admission can honor a
        // worktree-level restore reservation without a per-send DB query.
        worktree_id: repo::worktree_for(db, sess.direction_id, sess.repo_id)
            .await
            .ok()
            .flatten()
            .map(|w| w.id),
    };
    // Same persisted-meta restore as chat_open_worker_impl: this constructor
    // also races a fresh relaunch (slash discovery / direct chat_send), and an
    // engine born empty would persist a blank snapshot at its next checkpoint.
    engine::apply_persisted_meta(&mut inner, &sess.meta);
    let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    Ok(state.get_or_insert(session_id as i64, e))
}

#[tauri::command]
pub async fn chat_send(
    app: AppHandle,
    db: State<'_, Db>,
    session_id: i32,
    text: String,
    images: Option<Vec<ImageIn>>,
    files: Option<Vec<String>>,
) -> Result<(), String> {
    let eng = worker_engine(&app, &db, session_id)
        .await
        .map_err(|e| e.to_string())?;
    engine::send(
        &app,
        &db,
        &eng,
        &text,
        to_pairs(images),
        files.unwrap_or_default(),
        None,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn chat_interrupt(app: AppHandle, session_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::interrupt(&app, &eng)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_stop(app: AppHandle, session_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::stop(&app, &eng).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_dequeue(app: AppHandle, db: State<'_, Db>, session_id: i32, message_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::queue_remove(&app, &db, &eng, message_id).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_edit_queued(app: AppHandle, db: State<'_, Db>, session_id: i32, message_id: i32, text: String) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::queue_edit(&app, &db, &eng, message_id, &text).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_reorder_queue(app: AppHandle, db: State<'_, Db>, session_id: i32, order: Vec<i32>) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::queue_reorder(&app, &db, &eng, order).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_rewind(
    app: AppHandle,
    db: State<'_, Db>,
    session_id: i32,
    message_id: i32,
    mode: String,
) -> Result<engine::RewindOutcome, String> {
    let mode = engine::RewindMode::parse(&mode)?;
    // worker_engine (not a bare state lookup) so a restored session's engine
    // is rebuilt first — rewind must work right after an app relaunch too.
    let eng = worker_engine(&app, &db, session_id)
        .await
        .map_err(|e| e.to_string())?;
    engine::rewind(&app, &db, &eng, message_id, mode)
        .await
        .map_err(|e| e.to_string())
}

/// Lead-console conversation rewind (also covers curator threads — they run
/// lead-kind engines keyed by -thread_id). Conversation only: the lead has no
/// worktree, so there is no code half to restore.
#[tauri::command]
pub async fn lead_rewind(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    message_id: i32,
    lang: Option<String>,
) -> Result<engine::RewindOutcome, String> {
    // lead_engine (not a bare state lookup) so the engine is rebuilt first —
    // rewind must work right after an app relaunch too.
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::rewind(&app, &db, &eng, message_id, engine::RewindMode::Conversation)
        .await
        .map_err(|e| e.to_string())
}

/// The three coding-agent identities weft actually drives. `switch_lead_tool`/
/// `switch_worker_tool` reject anything else up front — a typo'd tool name
/// must fail loudly here, not deep inside a spawn as a raw "No such file".
const KNOWN_TOOLS: &[&str] = &["claude", "codex", "opencode"];

/// Blank → None (a cleared/never-set override), trimmed otherwise. Mirrors how
/// `set_tool_command` treats an empty alias as "no override" rather than
/// literally pinning to the empty string.
fn normalize_model(model: Option<String>) -> Option<String> {
    model.and_then(|m| {
        let m = m.trim();
        if m.is_empty() {
            None
        } else {
            Some(m.to_string())
        }
    })
}

/// Result of switching a lead/worker's engine identity and/or model override
/// (issue #96/#98). Both success and failure must be honestly visible to the
/// user (not a bare boolean) — the frontend renders the concrete before/after
/// (e.g. "claude → codex", "no override → gpt-5.5-high").
#[derive(serde::Serialize, Clone)]
pub struct SwitchOutcome {
    pub old_tool: String,
    pub new_tool: String,
    pub old_model: Option<String>,
    pub new_model: Option<String>,
    /// `None` for a human-initiated switch (Settings/Session panel); a short
    /// machine tag (currently only `"quota_exceeded"`, issue #97) when Weft
    /// triggered the switch itself — the frontend marker renders this
    /// distinctly so an auto fail-over is never mistaken for something the
    /// user clicked.
    pub reason: Option<String>,
    /// Structured basis for an automatic quota switch. Human switches leave
    /// this absent; the frontend localizes the stable value instead of parsing
    /// free-form error text.
    pub quota_basis: Option<String>,
}

/// A durable, visible timeline marker for a switch — same "system-owned,
/// always part of the record" treatment `insert_rewind_marker` gives a
/// rewind. `session_id` is None for the lead, Some for a worker.
async fn insert_switch_marker(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    outcome: &SwitchOutcome,
) {
    let turn_id = repo::next_turn_id(db, thread_id).await.unwrap_or(1);
    let content = serde_json::json!({
        "old_tool": outcome.old_tool,
        "new_tool": outcome.new_tool,
        "old_model": outcome.old_model,
        "new_model": outcome.new_model,
        "reason": outcome.reason,
        "quota_basis": outcome.quota_basis,
    })
    .to_string();
    match repo::insert_lead_message(
        db,
        thread_id,
        session_id,
        turn_id,
        "system",
        "engine_switch",
        &content,
        "complete",
    )
    .await
    {
        Ok(m) => {
            let _ = app.emit(engine::EVENT, engine::Push::Message { thread_id, message: m });
        }
        Err(e) => eprintln!("[weft] engine-switch marker insert failed: {e}"),
    }
}

fn emit_engine_switched(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    direction_id: Option<i32>,
    outcome: &SwitchOutcome,
    command: Option<String>,
) {
    let _ = app.emit(
        engine::EVENT,
        engine::Push::EngineSwitched {
            thread_id,
            session_id,
            direction_id,
            tool: outcome.new_tool.clone(),
            model: outcome.new_model.clone(),
            command,
        },
    );
}

/// Stable, locale-independent code a failed switch rejects with, matched by
/// `src/session/engineSwitch.ts` so the dialog renders translated copy from
/// `src/i18n/{en,zh}.ts` instead of raw SQLite text. Same contract as
/// `process_quota::DEGRADED_ERROR_CODE`: reject with the CODE, log the detail.
///
/// The atomic write leaves exactly ONE failure mode — the transaction did not
/// commit, so no tool change, no cleared native id, no marker. Two codes all
/// the same, because the user-visible STATE differs by something the failure
/// mode does not capture: whether the switch had already torn a live engine
/// down before it failed. `teardown_for_switch` only runs when
/// `LeadChatState::remove` returns one, and switching an idle or never-opened
/// surface — the common case — interrupts nothing at all.
///
/// That distinction is the whole reason for the split. Copy that claims an
/// interruption on an idle switch is as wrong as copy that claims nothing
/// changed after a real one; this PR's review history contains four instances
/// of the second mistake and one of the first, which is what a single
/// unconditional sentence buys.
///
/// Earlier revisions carried three codes, one per failure mode of a multi-step
/// design. That count remains a decent smell test — but count the states the
/// USER can be left in, not the ways the code can fail.
pub const SWITCH_FAILED_ERROR_CODE: &str = "switch_failed";
/// As above, when a live engine WAS torn down first: its turn was killed and
/// its open and queued rows finalized as `interrupted`.
pub const SWITCH_FAILED_INTERRUPTED_ERROR_CODE: &str = "switch_failed_interrupted";

/// Reject with the code matching what the user is actually left with, and log
/// the cause. `interrupted` is whether this command tore a live engine down
/// before the write failed.
fn switch_failure(surface: &str, interrupted: bool, err: impl std::fmt::Display) -> String {
    eprintln!("[weft] engine switch failed for {surface}; nothing was persisted: {err}");
    if interrupted {
        return SWITCH_FAILED_INTERRUPTED_ERROR_CODE.to_string();
    }
    SWITCH_FAILED_ERROR_CODE.to_string()
}

/// Switch the LEAD's engine identity and/or model override for `thread_id` —
/// issue #96 layer 1 of 3 (independent of any worker's tool and of the global
/// default; see `switch_worker_tool` / `set_default_tool`). Semantics:
///   1. cancel any ask still pending for this thread's lead
///      (`AskRegistry::cancel_for`) — it can never be answered by the engine
///      that is about to be torn down.
///   2. tear down the live engine, if any — same child-kill + row
///      finalization as Stop, but landing on "idle" rather than
///      `STATUS_STOPPED` (`engine::teardown_for_switch`), since this is a
///      replacement, not a stop the user needs to explicitly resume from.
///   3. make the switch durable in ONE transaction
///      (`repo::switch_lead_engine_txn`): the new tool/model (which also
///      clears any stale command-alias pin), the native session id
///      cleared (dogfooding pitfall #1: a stale native id handed to a
///      different engine's `--resume`/`resume` fails fast with "No
///      conversation found"), and the freeze-recovery grace marker that
///      vouches for that clear. The marker and the clear are the two halves of
///      `revive::has_resumable_context`; committing them together is what makes
///      the defect this fixes impossible rather than merely guarded, and is
///      why none of the gating/retraction machinery earlier revisions of this
///      PR carried is present. See `repo::mark_turn_freeze_recovered` for why a
///      switch writes that marker at all.
///   4. reconstruct the engine fresh via `lead_engine` — the exact construction
///      path a cold app boot uses (re-injects the ask-hook/MCP servers/
///      system-prompt for the NEW tool identity), never a hand-patched partial
///      update that could drift from it.
///   5. stage a mechanical history digest (`engine::build_switch_digest`) to
///      ride the new engine's first turn — dogfooding pitfall #2, "new engine
///      can't see thread history".
///   6. insert a durable, visible timeline marker so the switch — success or
///      (via the `?` below) failure surfaced to the caller — is honest either
///      way.
/// Switching to the SAME tool (optionally with a different/cleared model) is
/// allowed on purpose: it is the only lever weft gives to force a stuck/wedged
/// engine to restart AND pick up an externally-edited CLI config (e.g. the
/// user changed `~/.claude/settings.json`'s model) without a full app restart.
#[tauri::command]
pub async fn switch_lead_tool(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    tool: String,
    model: Option<String>,
    lang: Option<String>,
) -> Result<SwitchOutcome, String> {
    switch_lead_tool_inner(&app, &db, thread_id, tool, model, lang, None, None).await
}

/// The body of [`switch_lead_tool`], taking plain refs instead of Tauri's
/// `State` extractor so non-IPC backend code can call it too — currently the
/// only other caller is [`maybe_failover_on_quota`] (issue #97: auto fail-over
/// on a quota-exceeded turn reuses this EXACT sequence, not a hand-rolled
/// partial update). See `switch_lead_tool`'s original doc for the full
/// six-step rationale; this split is purely mechanical (no behavior change).
/// `reason` rides straight onto the returned/persisted [`SwitchOutcome`] — see
/// its doc.
async fn switch_lead_tool_inner(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    tool: String,
    model: Option<String>,
    lang: Option<String>,
    reason: Option<String>,
    quota_basis: Option<String>,
) -> Result<SwitchOutcome, String> {
    if !KNOWN_TOOLS.contains(&tool.as_str()) {
        return Err(format!("unknown tool {tool:?}"));
    }
    let model = normalize_model(model);
    let automatic_failover = reason.as_deref() == Some(QUOTA_FAILOVER_REASON);
    // Serialize a human-requested switch with the final quota-failover
    // handoff. The gate covers the initial DB snapshot as well as teardown, so
    // an automatic switch cannot commit from an older route after a human has
    // started choosing a new one.
    let _switch_gate = if automatic_failover {
        None
    } else {
        Some(engine_switch_gate(lead_key(thread_id)).lock_owned().await)
    };
    if !automatic_failover {
        if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
            if eng.lock().await.quota_failover_committing {
                return Err("engine_switch_in_progress".to_string());
            }
        }
    }
    let before = repo::get_thread(db, thread_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("thread {thread_id} not found"))?;

    // The lead's OWN timeline only — `list_lead_messages` returns every row for
    // the thread, including every worker's chat (session_id = Some(_)); the
    // lead's digest must not get polluted with unrelated worker conversations
    // (mirrors LeadTab.tsx's own `session_id == null` filter for what the lead
    // console renders).
    let messages: Vec<_> = repo::list_lead_messages(db, thread_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.session_id.is_none())
        .collect();
    let digest = engine::build_switch_digest(&before.lead_tool, &tool, &messages);

    if automatic_failover
        && !quota_failover_commit_is_valid(app, thread_id, None, &before.lead_tool).await
    {
        return Err(QUOTA_FAILOVER_CANCELED.to_string());
    }
    if let Some(asks) = app.try_state::<crate::ask::AskRegistry>() {
        asks.cancel_for(thread_id, "lead");
    }

    // ONE transaction: the new tool/model, the native-id clear (issue #96
    // pitfall 1) and the grace marker that vouches for it. The marker and the
    // clear are the two halves of `revive::has_resumable_context`, and
    // committing them together is what makes the defect this PR fixes
    // structurally impossible rather than merely guarded.
    let pinned = reason.is_none();
    // A fail-over keeps its committing flag in the old engine until the durable
    // route is updated. That prevents a concurrent open from constructing an
    // old-tool engine in the remove-to-write window below.
    if automatic_failover {
        repo::switch_lead_engine_txn_with_pin(db, thread_id, &tool, model.as_deref(), pinned)
            .await
            .map_err(|e| switch_failure(&format!("thread {thread_id}"), false, e))?;
    }

    // Whether real work was interrupted decides which failure the user is
    // shown if the write below does not commit — see `switch_failure`. The
    // answer comes from the teardown itself, NOT from whether an engine was
    // cached: a resident-but-idle engine is torn down with nothing to cut
    // short (review round 13).
    let mut interrupted = false;
    if let Some(eng) = app.state::<LeadChatState>().remove(lead_key(thread_id)) {
        interrupted = engine::teardown_for_switch(app, &eng).await;
    }
    if !automatic_failover {
        repo::switch_lead_engine_txn_with_pin(db, thread_id, &tool, model.as_deref(), pinned)
            .await
            .map_err(|e| switch_failure(&format!("thread {thread_id}"), interrupted, e))?;
    }

    let outcome = SwitchOutcome {
        old_tool: before.lead_tool,
        new_tool: tool,
        old_model: before.lead_model,
        new_model: model,
        reason,
        quota_basis,
    };
    let lang = lang.unwrap_or_else(|| "en".to_string());
    let eng = match lead_engine(app, db, thread_id, &lang).await {
        Ok(eng) => eng,
        Err(err) if automatic_failover => {
            // The durable transaction already committed. Treat a failed eager
            // reconstruction as a committed failover (a later open can retry
            // construction) rather than writing a false failure marker and
            // leaving the frontend on the exhausted provider.
            eprintln!(
                "[weft][quota] committed lead fail-over for thread {thread_id}, but eager engine reconstruction failed: {err}"
            );
            insert_switch_marker(app, db, thread_id, None, &outcome).await;
            emit_engine_switched(app, thread_id, None, None, &outcome, None);
            return Ok(outcome);
        }
        Err(err) => return Err(err.to_string()),
    };
    if !digest.is_empty() {
        eng.lock().await.pending_context_digest = Some(digest);
    }

    insert_switch_marker(app, db, thread_id, None, &outcome).await;
    emit_engine_switched(app, thread_id, None, None, &outcome, None);
    Ok(outcome)
}

/// Switch a WORKER's engine identity and/or model override for `session_id` —
/// issue #96 layer 2 of 3 (independent of the thread's lead and of the global
/// default; see `switch_lead_tool` / `set_default_tool`). Same six-step
/// semantics as `switch_lead_tool` — including step 3's single transaction,
/// which `repo::switch_worker_engine_txn` provides rather than this restating
/// it — with two worker-specific differences:
///   - the tool/model write is `repo::switch_worker_engine_txn` — ONE transaction
///     covering BOTH `direction.tool` (the durable side: `chat_open_worker_impl`'s
///     cold-recreate path, which fires the very next time this worker is
///     opened since the native id this switch just cleared makes that
///     function's resume condition false, reads `direction.tool` not
///     `session.tool`) and `session.tool`/`session.model` (the live side
///     `worker_engine`/every `chat_send` reads) — not two independent writes.
///     A torn write between them would leave those two readers disagreeing
///     about which tool this worker is actually running: the panel shows the
///     new tool, the next message silently goes to the old one — #96's core
///     confusion recurring in a harder-to-notice shape.
///   - the history digest is built from ONLY this session's own rows (a
///     worker's timeline is a sub-thread of the lead's, keyed by
///     `session_id`), not the whole thread's.
#[tauri::command]
pub async fn switch_worker_tool(
    app: AppHandle,
    db: State<'_, Db>,
    session_id: i32,
    tool: String,
    model: Option<String>,
) -> Result<SwitchOutcome, String> {
    switch_worker_tool_inner(&app, &db, session_id, tool, model, None, None).await
}

/// The body of [`switch_worker_tool`] — see [`switch_lead_tool_inner`]'s doc
/// for why this split exists (issue #97's [`maybe_failover_on_quota`] is the
/// other caller). Purely mechanical split, no behavior change.
async fn switch_worker_tool_inner(
    app: &AppHandle,
    db: &Db,
    session_id: i32,
    tool: String,
    model: Option<String>,
    reason: Option<String>,
    quota_basis: Option<String>,
) -> Result<SwitchOutcome, String> {
    if !KNOWN_TOOLS.contains(&tool.as_str()) {
        return Err(format!("unknown tool {tool:?}"));
    }
    let model = normalize_model(model);
    let automatic_failover = reason.as_deref() == Some(QUOTA_FAILOVER_REASON);
    // See the lead equivalent: manual changes and automatic quota handoffs
    // share one per-engine gate from their first snapshot through replacement.
    let _switch_gate = if automatic_failover {
        None
    } else {
        Some(engine_switch_gate(session_id as i64).lock_owned().await)
    };
    if !automatic_failover {
        if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
            if eng.lock().await.quota_failover_committing {
                return Err("engine_switch_in_progress".to_string());
            }
        }
    }
    let sess = repo::get_session(db, session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("session {session_id} not found"))?;
    let dir = repo::get_direction(db, sess.direction_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("direction {} not found", sess.direction_id))?;

    let messages = repo::list_lead_messages(db, dir.thread_id).await.unwrap_or_default();
    let own: Vec<_> = messages
        .into_iter()
        .filter(|m| m.session_id == Some(session_id))
        .collect();
    let digest = engine::build_switch_digest(&sess.tool, &tool, &own);

    if automatic_failover
        && !quota_failover_commit_is_valid(app, dir.thread_id, Some(session_id), &sess.tool).await
    {
        return Err(QUOTA_FAILOVER_CANCELED.to_string());
    }
    if let Some(asks) = app.try_state::<crate::ask::AskRegistry>() {
        asks.cancel_for(dir.thread_id, &sess.direction_id.to_string());
    }

    // ONE transaction, worker axis — see switch_lead_tool. It already covered
    // both halves of the tool write itself (`direction.tool` for the
    // cold-recreate path, `session.tool`/`model` for the live one), which is
    // why the marker and the native-id clear simply joined it.
    let pinned = reason.is_none();
    if automatic_failover {
        repo::switch_worker_engine_txn_with_pin(
            db,
            sess.direction_id,
            session_id,
            &tool,
            model.as_deref(),
            pinned,
        )
        .await
        .map_err(|e| switch_failure(&format!("session {session_id}"), false, e))?;
    }

    // Same as switch_lead_tool: the teardown reports, cache presence does not.
    let mut interrupted = false;
    if let Some(eng) = app.state::<LeadChatState>().remove(session_id as i64) {
        interrupted = engine::teardown_for_switch(app, &eng).await;
    }
    if !automatic_failover {
        repo::switch_worker_engine_txn_with_pin(
            db,
            sess.direction_id,
            session_id,
            &tool,
            model.as_deref(),
            pinned,
        )
        .await
        .map_err(|e| switch_failure(&format!("session {session_id}"), interrupted, e))?;
    }

    let outcome = SwitchOutcome {
        old_tool: sess.tool,
        new_tool: tool,
        old_model: sess.model,
        new_model: model,
        reason,
        quota_basis,
    };
    let eng = match worker_engine(app, db, session_id).await {
        Ok(eng) => eng,
        Err(err) if automatic_failover => {
            eprintln!(
                "[weft][quota] committed worker fail-over for session {session_id}, but eager engine reconstruction failed: {err}"
            );
            insert_switch_marker(app, db, dir.thread_id, Some(session_id), &outcome).await;
            emit_engine_switched(
                app,
                dir.thread_id,
                Some(session_id),
                Some(sess.direction_id),
                &outcome,
                Some(crate::tool_command::effective(None, &outcome.new_tool)),
            );
            return Ok(outcome);
        }
        Err(err) => return Err(err.to_string()),
    };
    if !digest.is_empty() {
        eng.lock().await.pending_context_digest = Some(digest);
    }

    insert_switch_marker(app, db, dir.thread_id, Some(session_id), &outcome).await;
    emit_engine_switched(
        app,
        dir.thread_id,
        Some(session_id),
        Some(sess.direction_id),
        &outcome,
        Some(crate::tool_command::effective(None, &outcome.new_tool)),
    );
    Ok(outcome)
}

// ───────────────────────── issue #97: quota fail-over ─────────────────────────

/// app_setting key gating auto fail-over on a quota-exceeded turn (see
/// [`maybe_failover_on_quota`]). Unset/anything else = DISABLED — this is
/// opt-in: switching engines mid-task ships that engine's own history digest
/// to a DIFFERENT provider, a cost/privacy tradeoff only the user should turn
/// on (surfaced as a toggle in Settings → Automation).
pub const K_QUOTA_FAILOVER_ENABLED: &str = crate::engine_routing::K_QUOTA_FAILOVER_ENABLED;

/// The reason tag [`maybe_failover_on_quota`] stamps on the [`SwitchOutcome`]
/// it produces, so the timeline marker (and anything else reading
/// `engine_switch` rows) can tell an auto fail-over apart from a switch the
/// user clicked.
pub const QUOTA_FAILOVER_REASON: &str = "quota_exceeded";

/// Internal cancellation sentinel: a user started a new turn, stopped the
/// engine, or changed its identity before the background fail-over reached its
/// final safe handoff. This is not a failed switch and must not create a
/// misleading failure marker.
const QUOTA_FAILOVER_CANCELED: &str = "quota_failover_canceled";

/// Minimum gap between two auto fail-overs of the SAME (thread, session) —
/// the storm-breaker alongside the "fallback isn't ALSO exceeded" check in
/// [`maybe_failover_on_quota`]: a flapping quota reading, or two failed turns
/// landing close together before the first switch's teardown even finishes,
/// must not turn into a rapid-fire switch loop.
const QUOTA_FAILOVER_COOLDOWN_SECS: u64 = 120;

type QuotaFailoverKey = (i32, Option<i32>);

fn quota_failover_cooldowns() -> &'static std::sync::Mutex<std::collections::HashMap<QuotaFailoverKey, std::time::Instant>>
{
    static COOLDOWNS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<QuotaFailoverKey, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    COOLDOWNS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Read-only peek used before resolving the shared routing decision. The
/// mutating claim still happens immediately before dispatch so two concurrent
/// turn-end callbacks cannot both switch the same session.
fn quota_failover_cooldown_active(key: QuotaFailoverKey) -> bool {
    let cooldowns = quota_failover_cooldowns()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cooldowns
        .get(&key)
        .is_some_and(|last| last.elapsed().as_secs() < QUOTA_FAILOVER_COOLDOWN_SECS)
}

/// Claims the fail-over slot for `key` (thread_id, session_id) iff it hasn't
/// already fired within [`QUOTA_FAILOVER_COOLDOWN_SECS`] — a one-shot gate,
/// not a queue: a call that returns `false` should just skip this round
/// entirely, not retry.
fn claim_quota_failover_slot(key: QuotaFailoverKey) -> bool {
    let mut cooldowns = quota_failover_cooldowns()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if let Some(last) = cooldowns.get(&key) {
        if now.duration_since(*last).as_secs() < QUOTA_FAILOVER_COOLDOWN_SECS {
            return false;
        }
    }
    cooldowns.insert(key, now);
    true
}

fn release_quota_failover_slot(key: QuotaFailoverKey) {
    let mut cooldowns = quota_failover_cooldowns()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cooldowns.remove(&key);
}

/// Gate 6 of the fail-over decision — the concurrency claim on the cooldown
/// slot, resolved at the very last moment before the engine handoff (see the
/// call site in [`maybe_failover_on_quota`], AFTER the per-engine switch gate
/// is held and the manual-pin recheck has passed).
///
/// [`crate::engine_routing::FailoverDecision`] only models gates 1-5
/// (enabled / structured-exceeded / fallback known / fallback not ALSO
/// exceeded / cooldown not active) because it is resolved once, early,
/// against a cheap pre-switch-gate peek ([`quota_failover_cooldown_active`]).
/// That peek can go stale while the caller waits on the switch gate — a
/// second, concurrent `maybe_failover_on_quota` invocation for the same key
/// can claim the slot in the meantime — so this gate has to be its own,
/// later, separately-resolved step, exactly like the manual-pin gate already
/// gets both an early check (folded into `FailoverDecision`'s inputs) AND a
/// late [`quota_failover_still_unpinned`] recheck.
///
/// This deliberately does NOT split "judge eligibility" from "perform the
/// claim" into two independent calls (a peek-based pure predicate plus a
/// separately-invoked mutating claim): two concurrent callers could both see
/// a pre-claim peek as "available" and only find out they collided when a
/// SECOND, unrelated claim call ran later — a real TOCTOU window, and a trap
/// for a future caller who trusts the peek instead of the claim's own return
/// value. Instead, [`claim_quota_failover_slot`] remains the single atomic
/// compare-and-swap (lock, check elapsed-since-last-claim, insert `now` — all
/// under one mutex acquisition) that decides for real; this function only
/// folds its already-resolved, authoritative result into an
/// exhaustively-matched verdict the call site must handle. There is no
/// window in which two concurrent calls can both observe `Claimed` for the
/// same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaFailoverClaimGate {
    /// This call won the race: the slot is now ours for
    /// [`QUOTA_FAILOVER_COOLDOWN_SECS`].
    Claimed,
    /// Another call already holds the slot for this key — skip this round
    /// entirely rather than retry (see [`claim_quota_failover_slot`]'s doc).
    Lost,
}

#[must_use]
fn quota_failover_claim_gate(key: QuotaFailoverKey) -> QuotaFailoverClaimGate {
    if claim_quota_failover_slot(key) {
        QuotaFailoverClaimGate::Claimed
    } else {
        QuotaFailoverClaimGate::Lost
    }
}

fn quota_failover_engine_key(thread_id: i32, session_id: Option<i32>) -> i64 {
    session_id.map(i64::from).unwrap_or_else(|| lead_key(thread_id))
}

/// Per-engine switch serialization. A manual switch takes this before reading
/// its durable route; quota failover takes it before claiming the final idle
/// handoff. Keeping the guard outside EngineInner means it remains effective
/// while a switch temporarily removes the cached engine to rebuild it.
fn engine_switch_gate(engine_key: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static GATES: std::sync::OnceLock<
        dashmap::DashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>,
    > = std::sync::OnceLock::new();
    let gates = GATES.get_or_init(dashmap::DashMap::new);
    gates
        .entry(engine_key)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn can_commit_quota_failover(inner: &engine::EngineInner, expected_tool: &str) -> bool {
    !inner.quota_failover_committing
        && !inner.turn.busy
        && !inner.interrupting
        && !inner.stopped
        && inner.tool == expected_tool
}

async fn claim_quota_failover_commit(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    expected_tool: &str,
) -> bool {
    let state = app.state::<LeadChatState>();
    let Some(eng) = state.get(quota_failover_engine_key(thread_id, session_id)) else {
        return false;
    };
    let mut inner = eng.lock().await;
    if !can_commit_quota_failover(&inner, expected_tool) {
        return false;
    }
    inner.quota_failover_committing = true;
    true
}

async fn quota_failover_commit_is_valid(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    expected_tool: &str,
) -> bool {
    let state = app.state::<LeadChatState>();
    let Some(eng) = state.get(quota_failover_engine_key(thread_id, session_id)) else {
        return false;
    };
    let inner = eng.lock().await;
    inner.quota_failover_committing
        && !inner.turn.busy
        && !inner.interrupting
        && !inner.stopped
        && inner.tool == expected_tool
}

async fn release_quota_failover_commit(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
) {
    let state = app.state::<LeadChatState>();
    let Some(eng) = state.get(quota_failover_engine_key(thread_id, session_id)) else {
        return;
    };
    eng.lock().await.quota_failover_committing = false;
}

/// The policy decision is made before waiting on the per-engine switch gate.
/// A human can complete a same-tool manual switch in that interval, so re-read
/// the durable pin once the gate is ours. A read failure remains fail-closed.
async fn quota_failover_still_unpinned(db: &Db, thread_id: i32, session_id: Option<i32>) -> bool {
    match crate::engine_routing::has_manual_pin(db, thread_id, session_id).await {
        Ok(false) => true,
        Ok(true) => false,
        Err(err) => {
            eprintln!("[weft][quota] skipped fail-over because post-gate pin lookup failed: {err}");
            false
        }
    }
}

/// Fire-and-forget entry point: engine.rs's two TurnEnd sites (codex_consumer,
/// spawn_reader) call this right after a turn ends in error (issue #97).
/// Spawns its OWN task so it can safely re-lock the engine —
/// `switch_lead_tool_inner`/`switch_worker_tool_inner` tear down and
/// reconstruct it — without deadlocking the CALLER, which may still be
/// holding that very lock (a `tokio::sync::Mutex` is not reentrant).
pub fn spawn_quota_failover_check(
    app: AppHandle,
    db: Db,
    thread_id: i32,
    session_id: Option<i32>,
    tool: String,
    structured_exceeded: bool,
) {
    if !structured_exceeded {
        return;
    }
    tauri::async_runtime::spawn(async move {
        maybe_failover_on_quota(&app, &db, thread_id, session_id, &tool, structured_exceeded)
            .await;
    });
}

/// issue #97: auto-switch a thread/session away from `tool` using the shared
/// engine-routing resolver. Thin wiring only — resolve the impure inputs
/// (setting, installed tools, structured quota readings, cooldown peek),
/// delegate the judgment call to the shared function (gates 1-5), then a
/// second, later, exhaustively-matched gate (6: [`quota_failover_claim_gate`])
/// governs dispatch. Reuses
/// [`switch_lead_tool_inner`]/[`switch_worker_tool_inner`] VERBATIM — the
/// exact six-step teardown/persist/reconstruct sequence #139 shipped and
/// reviewed, never a hand-rolled partial tool/model update. Every failure is
/// logged AND left as a durable, visible marker (issue #97 review P2) —
/// best-effort throughout, since nothing is waiting on this fire-and-forget
/// check.
async fn maybe_failover_on_quota(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    tool: &str,
    structured_exceeded: bool,
) {
    let key: QuotaFailoverKey = (thread_id, session_id);
    let cooldown_ok = !quota_failover_cooldown_active(key);
    let manual_pin = match crate::engine_routing::has_manual_pin(db, thread_id, session_id).await {
        Ok(manual_pin) => manual_pin,
        Err(err) => {
            eprintln!("[weft][quota] skipped fail-over because pin lookup failed: {err}");
            return;
        }
    };
    let decision = crate::engine_routing::quota_failover_for_db(
        db,
        tool,
        manual_pin,
        structured_exceeded,
        cooldown_ok,
    )
    .await;
    let fallback = match decision {
        crate::engine_routing::FailoverDecision::Skip(_) => return,
        crate::engine_routing::FailoverDecision::Blocked(reason) => {
            let fallback = match crate::engine_routing::EngineId::parse(tool) {
                Some(crate::engine_routing::EngineId::Claude) => Some("codex"),
                Some(crate::engine_routing::EngineId::Codex) => Some("claude"),
                _ => None,
            };
            let reason = match reason {
                crate::engine_routing::FailoverBlockedReason::FallbackUnavailable => {
                    "automatic_candidate_unavailable"
                }
                crate::engine_routing::FailoverBlockedReason::BothAutomaticCandidatesExceeded => {
                    "both_automatic_candidates_exceeded"
                }
            };
            crate::engine_routing::record_failover_blocked(
                db,
                thread_id,
                session_id,
                tool,
                fallback,
                reason,
            )
            .await;
            return;
        }
        crate::engine_routing::FailoverDecision::SwitchTo { tool, .. } => tool.as_str(),
    };
    // A manual switch that arrived first holds this through its durable write
    // and engine rebuild. If we acquire it first, the later manual call waits,
    // then reads the post-failover state rather than racing a stale snapshot.
    let _switch_gate = engine_switch_gate(quota_failover_engine_key(thread_id, session_id))
        .lock_owned()
        .await;
    if !quota_failover_still_unpinned(db, thread_id, session_id).await {
        return;
    }
    // Gate 6 — claim the cooldown slot before the short final engine
    // handoff, so a second turn-end callback cannot race this one into a
    // duplicate switch. See `quota_failover_claim_gate`'s doc for why this
    // is its own, later, exhaustively-matched step rather than folded into
    // `FailoverDecision`'s gates 1-5. `quota_failover_claim_gate_blocks_a_
    // concurrent_claim` below calls this EXACT function and proves its
    // claim/reclaim behavior directly. What no test proves — the same,
    // disclosed limitation every gate here shares — is that this call site
    // itself still runs: `maybe_failover_on_quota` needs an `AppHandle`,
    // unavailable in this crate's test harness (same limitation the
    // `push_model_arg` tests' doc comment records). Deleting this `match`,
    // or replacing `quota_failover_claim_gate` with a stub that always
    // returns `Claimed`, would silently defeat
    // `QUOTA_FAILOVER_COOLDOWN_SECS`'s storm-breaker promise without
    // failing the build.
    match quota_failover_claim_gate(key) {
        QuotaFailoverClaimGate::Lost => return,
        QuotaFailoverClaimGate::Claimed => {}
    }
    if !claim_quota_failover_commit(app, thread_id, session_id, tool).await {
        release_quota_failover_slot(key);
        return;
    }
    eprintln!(
        "[weft][quota] {tool} exceeded its usage limit — auto-switching to {fallback} (thread {thread_id}, session {session_id:?})"
    );
    let reason = Some(QUOTA_FAILOVER_REASON.to_string());
    let result = match session_id {
        Some(sid) => {
            switch_worker_tool_inner(
                app,
                db,
                sid,
                fallback.to_string(),
                None,
                reason,
                Some("exceeded".to_string()),
            )
                .await
                .map(|_| ())
        }
        None => {
            switch_lead_tool_inner(
                app,
                db,
                thread_id,
                fallback.to_string(),
                None,
                None,
                reason,
                Some("exceeded".to_string()),
            )
                .await
                .map(|_| ())
        }
    };
    if let Err(err) = result {
        release_quota_failover_commit(app, thread_id, session_id).await;
        if err == QUOTA_FAILOVER_CANCELED {
            release_quota_failover_slot(key);
            return;
        }
        eprintln!("[weft][quota] auto fail-over {tool} -> {fallback} failed: {err}");
        insert_quota_failover_failed_marker(app, db, thread_id, session_id, tool, fallback, &err)
            .await;
    }
}

/// issue #97 review P2: a failed auto fail-over attempt used to leave only an
/// `eprintln!` behind — indistinguishable, from the user's seat, from the
/// feature not existing at all. Durable, visible marker mirroring
/// `insert_switch_marker`'s "system-owned, always part of the record"
/// treatment for the SUCCESS case.
async fn insert_quota_failover_failed_marker(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    tool: &str,
    fallback: &str,
    error: &str,
) {
    let turn_id = repo::next_turn_id(db, thread_id).await.unwrap_or(1);
    let content = serde_json::json!({
        "tool": tool,
        "fallback": fallback,
        "error": error,
        "quota_basis": "exceeded",
    })
    .to_string();
    match repo::insert_lead_message(
        db,
        thread_id,
        session_id,
        turn_id,
        "system",
        "quota_failover_failed",
        &content,
        "complete",
    )
    .await
    {
        Ok(m) => {
            let _ = app.emit(engine::EVENT, engine::Push::Message { thread_id, message: m });
        }
        Err(e) => eprintln!("[weft] quota-failover-failed marker insert failed: {e}"),
    }
}

#[cfg(test)]
mod quota_failover_tests {
    use super::*;
    use crate::store::{repo, Db};

    #[test]
    fn quota_failover_cooldown_blocks_immediate_repeat_then_releases() {
        // Distinct key per test run (module-global hub) so parallel test
        // threads can't collide on the same cooldown slot.
        let key: QuotaFailoverKey = (900_001, None);
        assert!(!quota_failover_cooldown_active(key), "nothing claimed yet");
        assert!(claim_quota_failover_slot(key), "first claim should succeed");
        assert!(
            quota_failover_cooldown_active(key),
            "the peek must agree with the claim it just observed"
        );
        assert!(
            !claim_quota_failover_slot(key),
            "immediate second claim within the cooldown must be refused"
        );
        // A DIFFERENT key (e.g. a worker session on the same thread) is
        // independent — the cooldown is keyed, not global.
        let other: QuotaFailoverKey = (900_001, Some(1));
        assert!(!quota_failover_cooldown_active(other));
        assert!(claim_quota_failover_slot(other));
    }

    #[test]
    fn quota_failover_commit_requires_the_same_idle_engine() {
        let mut inner = engine::test_inner("codex");
        assert!(can_commit_quota_failover(&inner, "codex"));

        inner.turn.busy = true;
        assert!(!can_commit_quota_failover(&inner, "codex"));
        inner.turn.busy = false;

        inner.interrupting = true;
        assert!(!can_commit_quota_failover(&inner, "codex"));
        inner.interrupting = false;

        inner.stopped = true;
        assert!(!can_commit_quota_failover(&inner, "codex"));
        inner.stopped = false;

        assert!(!can_commit_quota_failover(&inner, "claude"));
        inner.quota_failover_committing = true;
        assert!(!can_commit_quota_failover(&inner, "codex"));
    }

    #[tokio::test]
    async fn engine_switch_gate_serializes_manual_and_quota_handoffs() {
        let key = -900_002;
        let first = engine_switch_gate(key).lock_owned().await;
        let second = engine_switch_gate(key);
        assert!(
            second.try_lock().is_err(),
            "a quota handoff must wait while a manual switch owns the same engine gate"
        );
        drop(first);
        assert!(
            second.try_lock().is_ok(),
            "the next switch may claim the gate after the first completes"
        );
    }

    #[tokio::test]
    async fn quota_failover_recheck_refuses_a_manual_pin_added_while_waiting() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();

        assert!(quota_failover_still_unpinned(&db, thread.id, None).await);
        repo::switch_lead_engine_txn_with_pin(&db, thread.id, "codex", None, true)
            .await
            .unwrap();
        assert!(
            !quota_failover_still_unpinned(&db, thread.id, None).await,
            "the post-gate recheck must stop the automatic handoff after a same-tool manual switch"
        );
    }

    /// The inline `matches!(.., Some("1") | Some("true"))` an earlier review
    /// pointed at (parsing [`K_QUOTA_FAILOVER_ENABLED`] right here in
    /// `maybe_failover_on_quota`) has since been extracted into
    /// `engine_routing::is_enabled` by the unrelated #111 routing refactor —
    /// a private fn in a module this change's scope does not touch. This
    /// pins down the same real source of truth ("did the user opt in?") end
    /// to end instead: the exact setting key, through the exact public entry
    /// point `maybe_failover_on_quota` calls, so a regression in that parser
    /// (or in the setting-key wiring) still fails a test living next to the
    /// feature it gates.
    #[tokio::test]
    async fn quota_failover_enabled_setting_gates_the_real_db_backed_decision() {
        // engine_quota is a process-global hub other tests also mutate.
        let _hub_lock = crate::engine_quota::hub_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::engine_quota::clear_for_test();

        let db = Db::connect("sqlite::memory:").await.unwrap();

        // Unset = disabled — opt-in feature, see K_QUOTA_FAILOVER_ENABLED's doc.
        assert_eq!(
            crate::engine_routing::quota_failover_for_db(&db, "claude", false, true, true).await,
            crate::engine_routing::FailoverDecision::Skip(
                crate::engine_routing::FailoverSkipReason::Disabled
            ),
            "unset setting must not silently enable an auto-switch"
        );

        repo::set_setting(&db, K_QUOTA_FAILOVER_ENABLED, "0").await.unwrap();
        assert_eq!(
            crate::engine_routing::quota_failover_for_db(&db, "claude", false, true, true).await,
            crate::engine_routing::FailoverDecision::Skip(
                crate::engine_routing::FailoverSkipReason::Disabled
            ),
            "\"0\" must not be read as truthy"
        );

        repo::set_setting(&db, K_QUOTA_FAILOVER_ENABLED, "1").await.unwrap();
        assert_eq!(
            crate::engine_routing::quota_failover_for_db(&db, "claude", false, true, true).await,
            crate::engine_routing::FailoverDecision::SwitchTo {
                tool: crate::engine_routing::EngineId::Codex,
                quota: None,
            },
            "\"1\" is documented as truthy and must clear the enabled gate"
        );

        crate::engine_quota::clear_for_test();
    }

    /// Complements `quota_failover_claim_gate_blocks_a_concurrent_claim`
    /// (which proves gate 6 in isolation) by proving how gate 6's real claim
    /// feeds back into gate 5's PEEK: this reproduces the exact
    /// peek -> decide -> claim -> peek -> decide sequence
    /// `maybe_failover_on_quota` performs across its two decision points,
    /// entirely through pure/sync pieces — once the slot is claimed, a
    /// second, concurrent caller's EARLY decision must flip from SwitchTo to
    /// Skip(CooldownActive) too, not just the late gate-6 claim itself. A
    /// full regression test through `maybe_failover_on_quota` itself needs
    /// an `AppHandle`, which this crate's test harness does not provide.
    #[test]
    fn quota_failover_dispatch_claim_blocks_a_concurrent_decision() {
        let key: QuotaFailoverKey = (900_010, None);
        let fallback = crate::engine_routing::RouteCandidate {
            tool: crate::engine_routing::EngineId::Claude,
            installed: true,
            quota: None,
        };

        let cooldown_ok = !quota_failover_cooldown_active(key);
        assert_eq!(
            crate::engine_routing::resolve_failover(
                crate::engine_routing::EngineId::Codex,
                true,
                false,
                true,
                Some(&fallback),
                cooldown_ok,
            ),
            crate::engine_routing::FailoverDecision::SwitchTo {
                tool: crate::engine_routing::EngineId::Claude,
                quota: None,
            },
            "first pass, nothing claimed yet, must resolve to a switch"
        );

        // The dispatch-time claim maybe_failover_on_quota performs right
        // before the actual engine handoff.
        assert!(claim_quota_failover_slot(key));

        let cooldown_ok_2 = !quota_failover_cooldown_active(key);
        assert_eq!(
            crate::engine_routing::resolve_failover(
                crate::engine_routing::EngineId::Codex,
                true,
                false,
                true,
                Some(&fallback),
                cooldown_ok_2,
            ),
            crate::engine_routing::FailoverDecision::Skip(
                crate::engine_routing::FailoverSkipReason::CooldownActive
            ),
            "a second concurrent decision must be blocked by the claim, not switch again"
        );
    }

    /// Direct proof of gate 6 (`quota_failover_claim_gate`) itself — unlike
    /// `quota_failover_dispatch_claim_blocks_a_concurrent_decision` above,
    /// which reconstructs the surrounding peek/decide sequence by hand, this
    /// calls the EXACT function `maybe_failover_on_quota` matches on at its
    /// dispatch point. If a future change made that function's internals
    /// vacuously return `Claimed` (bypassing the real
    /// `claim_quota_failover_slot` CAS), this test is what would catch it:
    /// a fresh key must win the claim; a second, concurrent call for the
    /// SAME key, before the cooldown elapses, must lose the race rather
    /// than claim again; a different key remains independent.
    #[test]
    fn quota_failover_claim_gate_blocks_a_concurrent_claim() {
        let key: QuotaFailoverKey = (900_020, None);
        assert_eq!(
            quota_failover_claim_gate(key),
            QuotaFailoverClaimGate::Claimed,
            "first call for a fresh key must win the claim"
        );
        assert_eq!(
            quota_failover_claim_gate(key),
            QuotaFailoverClaimGate::Lost,
            "a second concurrent call for the same key must lose the race, not re-claim"
        );

        // A different key (e.g. a worker session on the same thread) is
        // independent — same invariant `claim_quota_failover_slot` proves.
        let other: QuotaFailoverKey = (900_020, Some(1));
        assert_eq!(quota_failover_claim_gate(other), QuotaFailoverClaimGate::Claimed);
    }

}

#[tauri::command]
pub async fn lead_dequeue(app: AppHandle, db: State<'_, Db>, thread_id: i32, message_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::queue_remove(&app, &db, &eng, message_id).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn lead_edit_queued(app: AppHandle, db: State<'_, Db>, thread_id: i32, message_id: i32, text: String) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::queue_edit(&app, &db, &eng, message_id, &text).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn lead_reorder_queue(app: AppHandle, db: State<'_, Db>, thread_id: i32, order: Vec<i32>) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::queue_reorder(&app, &db, &eng, order).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// idle-time skill refresh (worker): re-inject the workspace's enabled skills
/// into the live session's cwd and flag the engine so the next send silently
/// restarts the resident process to pick them up. No-op if the engine is gone.
#[tauri::command]
pub async fn flag_session_skill_refresh(
    app: AppHandle,
    db: State<'_, Db>,
    session_id: i32,
) -> Result<(), String> {
    let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) else {
        return Ok(());
    };
    let (thread_id, cwd) = {
        let g = eng.lock().await;
        (g.thread_id, g.cwd.clone())
    };
    if let Ok(Some(th)) = repo::get_thread(&db, thread_id).await {
        crate::skills::inject_for(&db, th.workspace_id, &cwd).await;
    }
    eng.lock().await.pending_skill_refresh = true;
    Ok(())
}

/// idle-time skill refresh (lead). Same as the worker variant, keyed by thread.
#[tauri::command]
pub async fn flag_lead_skill_refresh(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<(), String> {
    let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) else {
        return Ok(());
    };
    let cwd = { eng.lock().await.cwd.clone() };
    if let Ok(Some(th)) = repo::get_thread(&db, thread_id).await {
        crate::skills::inject_for(&db, th.workspace_id, &cwd).await;
    }
    eng.lock().await.pending_skill_refresh = true;
    Ok(())
}

/// Frontend callback after a repo onboarding action card finishes (add /
/// new / clone). Wraps the payload in `<weft:repo_action>…</weft:repo_action>`
/// and delivers it as an invisible user turn so the agent can react without
/// the result polluting the visible timeline. Respects the turn machine:
/// mid-turn clicks get queued and flush at the next boundary instead of
/// shoving JSON between in-flight protocol lines. Best-effort by design:
/// stopped leads ignore this hidden feedback, while non-stopped missing
/// engines are recreated so fast empty-state clicks can still close the loop.
#[tauri::command]
pub async fn post_lead_tool_result(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    payload: serde_json::Value,
    lang: Option<String>,
) -> Result<bool, String> {
    post_lead_tool_result_inner(&app, &db, thread_id, payload, lang.as_deref().unwrap_or("en"))
        .await
}

/// The issue's test-case document for the panel (None = never derived).
#[tauri::command]
pub async fn get_test_plan(
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<Option<crate::store::entities::test_plan::Model>, String> {
    repo::get_test_plan(&db, thread_id)
        .await
        .map_err(|e| e.to_string())
}

/// Save a panel edit of the test-case document (source="user"). The caller
/// separately posts `test_cases_updated` via post_lead_tool_result so the lead
/// learns the new content — persisting must succeed even when the lead is
/// stopped, hence the two steps.
#[tauri::command]
pub async fn save_test_plan(
    db: State<'_, Db>,
    thread_id: i32,
    content: String,
) -> Result<(), String> {
    let content = content.trim();
    if content.is_empty() {
        return Err("test plan content must not be empty".into());
    }
    repo::upsert_test_plan(&db, thread_id, content, "user")
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Wrap a UI feedback payload in its sentinel tag for hidden delivery to the
/// lead. The tag follows `payload.tool`: `plan_decision` (plan-card approval)
/// and `test_cases_updated` (panel edit save) get their own tags; everything
/// else stays `repo_action` (the historical default the repo-onboarding flows
/// rely on).
fn hidden_feedback_text(payload: &serde_json::Value) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(payload)?;
    let tag = match payload.get("tool").and_then(|v| v.as_str()) {
        Some("plan_decision") => "plan_decision",
        Some("test_cases_updated") => "test_cases_updated",
        _ => "repo_action",
    };
    Ok(format!("<weft:{tag}>{json}</weft:{tag}>"))
}

/// Best-effort hidden delivery to the lead. Returns Ok(true) when the feedback
/// was handed to a live engine, Ok(false) when it was ignored (stopped lead,
/// dead engine, serialization failure) — callers that persist follow-up state
/// (e.g. collapsing an approved plan card) must check the flag so a dropped
/// delivery never masquerades as a received one.
///
/// A `plan_decision` (the user clicking Approve) REVIVES a stopped lead — the
/// click is an explicit "continue" intent, the same as sending a message — so
/// the approval isn't a dead-end that makes the user restart and re-approve.
/// Other feedback (test_cases_updated / repo_action) stays droppable when
/// stopped: its DB write is authoritative and the lead reads it back later.
async fn post_lead_tool_result_inner(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    payload: serde_json::Value,
    lang: &str,
) -> Result<bool, String> {
    let revives = payload.get("tool").and_then(|v| v.as_str()) == Some("plan_decision");
    let text = match hidden_feedback_text(&payload) {
        Ok(text) => text,
        Err(e) => {
            log_hidden_feedback_ignored(thread_id, &e.to_string());
            return Ok(false);
        }
    };
    let key = lead_key(thread_id);
    let eng = match app.state::<LeadChatState>().get(key) {
        Some(eng) => eng,
        None => {
            let stopped = matches!(
                repo::lead_status(db, thread_id)
                    .await
                    .ok()
                    .flatten()
                    .as_deref(),
                Some("stopped")
            );
            if stopped && !revives {
                log_hidden_feedback_ignored(thread_id, "lead is stopped");
                return Ok(false);
            }
            match lead_engine(app, db, thread_id, lang).await {
                Ok(eng) => eng,
                Err(e) => {
                    log_hidden_feedback_ignored(thread_id, &e.to_string());
                    return Ok(false);
                }
            }
        }
    };
    // A revive clears the stopped flag (like `send`) so the engine accepts the
    // hidden input; a non-revive only ensures an already-live engine is running.
    let ensured = if revives {
        engine::ensure_running_for_send(app, db, &eng).await
    } else {
        engine::ensure_running(app, db, &eng).await
    };
    if let Err(e) = ensured {
        log_hidden_feedback_ignored(thread_id, &e.to_string());
        return Ok(false);
    }
    if let Err(e) = engine::send_hidden_existing(app, db, &eng, text).await {
        log_hidden_feedback_ignored(thread_id, &e.to_string());
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod live_slot_tests {
    use super::{build_worker_slots, WorkerSnapshot};
    use crate::store::{repo, Db};

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    // workspace + repo_ref + thread + direction + a session row + a worktree row,
    // returning (thread_id, direction_id, repo_id, session_id).
    async fn fixture(db: &Db) -> (i32, i32, i32, i32) {
        let ws = repo::create_workspace(db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(db, ws.id, "r", "/tmp/weft-slot-fake", "main", "", true)
            .await
            .unwrap();
        let th = repo::create_thread(db, ws.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let dir = repo::create_direction(db, th.id, "alpha", "codex", repo_ref.id, "why", "impl-only", "")
            .await
            .unwrap();
        let sess = repo::create_session(db, dir.id, repo_ref.id, "codex", "/tmp/wt")
            .await
            .unwrap();
        repo::set_session_native_id(db, sess.id, "nat-1").await.unwrap();
        repo::record_worktree(db, repo_ref.id, dir.id, "feat/alpha", "/tmp/wt", false, true, "")
            .await
            .unwrap();
        (th.id, dir.id, repo_ref.id, sess.id)
    }

    fn snap(session_id: i32, thread_id: i32, busy: bool) -> WorkerSnapshot {
        WorkerSnapshot { session_id, thread_id, busy, queue: vec![] }
    }

    /// A busy worker is assembled into a full slot carrying its own thread id and
    /// DB-derived SessionInfo.
    #[tokio::test]
    async fn busy_worker_becomes_a_slot() {
        use crate::lead_chat::engine::QueuedItem;
        let db = mem().await;
        let (th, dir, repo_id, sess) = fixture(&db).await;

        let items = vec![
            QueuedItem { id: 10, text: "hi".into(), images: 0, files: 0, has_attachments: false },
            QueuedItem { id: 11, text: "there".into(), images: 1, files: 0, has_attachments: true },
        ];
        let slots = build_worker_slots(
            &db,
            vec![WorkerSnapshot { session_id: sess, thread_id: th, busy: true, queue: items }],
        )
        .await;

        assert_eq!(slots.len(), 1);
        let s = &slots[0];
        assert_eq!(s.info.session_id, sess);
        assert_eq!(s.direction_id, dir);
        assert_eq!(s.repo_id, repo_id);
        assert_eq!(s.thread_id, th);
        assert!(s.busy);
        assert_eq!(s.queue.len(), 2);
        assert_eq!(s.queue[0].id, 10);
        assert_eq!(s.queue[1].text.as_str(), "there");
        assert_eq!(s.info.repo, "/tmp/wt");
        assert_eq!(s.info.branch, "feat/alpha");
        assert_eq!(s.info.worktree, "/tmp/wt");
        assert_eq!(s.info.native_id.as_deref(), Some("nat-1"));
        assert!(s.info.resumed);
    }

    /// A NOT-busy snapshot (stopped/taken-over/idle-between-turns) is dropped — the
    /// busy filter is the whole defense against advertising a stopped engine.
    #[tokio::test]
    async fn idle_worker_is_excluded() {
        let db = mem().await;
        let (th, _dir, _repo_id, sess) = fixture(&db).await;

        let slots = build_worker_slots(&db, vec![snap(sess, th, false)]).await;

        assert!(slots.is_empty());
    }

    /// A snapshot whose session row is missing (e.g. a stale key) is skipped, not
    /// fatal — assembly is best-effort over the live set.
    #[tokio::test]
    async fn missing_session_row_is_skipped() {
        let db = mem().await;
        let (th, _dir, _repo_id, _sess) = fixture(&db).await;

        let slots = build_worker_slots(&db, vec![snap(999_999, th, true)]).await;

        assert!(slots.is_empty());
    }

    /// A busy worker without a materialized worktree row is skipped (worktree_for
    /// is None) rather than emitting a half-built slot.
    #[tokio::test]
    async fn busy_worker_without_worktree_is_skipped() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "r", "/tmp/x", "main", "", true).await.unwrap();
        let th = repo::create_thread(&db, ws.id, "issue", "feature", "codex").await.unwrap();
        let dir = repo::create_direction(&db, th.id, "alpha", "codex", repo_ref.id, "why", "impl-only", "")
            .await
            .unwrap();
        let sess = repo::create_session(&db, dir.id, repo_ref.id, "codex", "/tmp/wt")
            .await
            .unwrap();
        // NOTE: no record_worktree call.

        let slots = build_worker_slots(&db, vec![snap(sess.id, th.id, true)]).await;

        assert!(slots.is_empty());
    }
}

/// The engine/model switch's durable write — issue #96/#98, adversarial
/// re-review of PR #139 (P2).
///
/// The defect: `mark_turn_freeze_recovered` and `clear_native_id` were two
/// independent writes, so a failed marker write followed by a successful clear
/// left the surface at `native_id = None && marker = None` — the one
/// combination `revive::has_resumable_context` reads as "never ran", which
/// silently drops it out of the automated re-drive pool.
///
/// The fix is ONE transaction per axis (`repo::switch_lead_engine_txn` /
/// `switch_worker_engine_txn`) covering the tool/model change, the native-id
/// clear and the marker. They commit together or not at all, so the bad
/// combination cannot be observed. Nine rounds of review went into the
/// alternatives — stamping early and gating, retracting on failure, a pending
/// marker kind promoted at commit — and every one of them existed only to make
/// an EARLIER stamp safe. Atomicity removes the need for all of it.
///
/// SCOPE: these cover the `&Db` core. The `#[tauri::command]` wrappers are out
/// of reach (this crate has no `AppHandle` harness), so ask cancellation,
/// `teardown_for_switch`, the engine rebuild and the digest are NOT claimed as
/// covered — including which failure code a switch picks, since that keys on
/// `teardown_for_switch`'s return and both need a live `LeadChatState` — and neither are the concurrency windows between this command and
/// `revive`'s sweep or a concurrent `rewind`, which are pre-existing and
/// tracked separately. The transactions' own rollback proofs live in
/// `store::repo`'s tests, where a failure can be injected between the halves.
#[cfg(test)]
mod switch_write_tests {
    use crate::lead_chat::revive::has_resumable_context;
    use crate::store::{repo, Db};

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    /// (thread_id, direction_id, session_id) with a native id on BOTH axes, so
    /// each test can prove the clear happened.
    async fn fixture(db: &Db) -> (i32, i32, i32) {
        let ws = repo::create_workspace(db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(db, ws.id, "r", "/tmp/weft-switch", "main", "", true)
            .await
            .unwrap();
        let th = repo::create_thread(db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let dir = repo::create_direction(db, th.id, "alpha", "claude", repo_ref.id, "why", "impl-only", "")
            .await
            .unwrap();
        let sess = repo::create_session(db, dir.id, repo_ref.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        repo::set_lead_native_id(db, th.id, "lead-nat-1").await.unwrap();
        repo::set_session_native_id(db, sess.id, "worker-nat-1").await.unwrap();
        (th.id, dir.id, sess.id)
    }

    /// The code is a cross-language contract with
    /// `src/session/engineSwitch.ts`'s `SWITCH_FAILED_ERROR_CODE`, which no
    /// compiler checks across the boundary. Pins the Rust half: the rejection
    /// is the CODE alone, with the cause going to the log — renaming it here
    /// without updating the matcher and both locale files goes red on this
    /// side too.
    #[test]
    fn a_failed_switch_rejects_with_the_stable_code_only() {
        assert_eq!(
            super::switch_failure("thread 7", false, "database is locked"),
            "switch_failed",
            "spelling is mirrored in src/session/engineSwitch.ts — update both"
        );
        assert_eq!(
            super::switch_failure("thread 7", true, "database is locked"),
            "switch_failed_interrupted",
            "a switch that tore a live engine down first must say so"
        );
        assert_eq!(super::SWITCH_FAILED_ERROR_CODE, "switch_failed");
        assert_eq!(
            super::SWITCH_FAILED_INTERRUPTED_ERROR_CODE,
            "switch_failed_interrupted"
        );
        // One IS a prefix of the other, and the frontend matches by substring
        // — so `switchErrorCodeOf` must try the longest code first or an
        // interrupted switch silently renders the un-interrupted copy. Pinned
        // here because nothing checks that ordering across the boundary; if
        // this assertion ever has to change, that sort goes with it.
        assert!(
            super::SWITCH_FAILED_INTERRUPTED_ERROR_CODE
                .starts_with(super::SWITCH_FAILED_ERROR_CODE),
            "SWITCH_ERROR_I18N in src/session/engineSwitch.ts sorts by length for this reason"
        );
    }

    #[tokio::test]
    async fn a_lead_switch_lands_tool_clear_and_marker_together() {
        let db = mem().await;
        let (th, _dir, _sess) = fixture(&db).await;

        repo::switch_lead_engine_txn(&db, th, "codex", Some("gpt-5.5-high"))
            .await
            .unwrap();

        let t = repo::get_thread(&db, th).await.unwrap().unwrap();
        assert_eq!(t.lead_tool, "codex");
        assert_eq!(t.lead_model.as_deref(), Some("gpt-5.5-high"));
        let native = repo::lead_native_id(&db, th).await.unwrap();
        let recovered = repo::last_turn_freeze_recovery_secs(&db, th, None).await.unwrap();
        assert_eq!(native, None, "issue #96 pitfall 1: the old engine's id must not survive");
        assert!(recovered.is_some(), "and the marker that vouches for it is in the same commit");
        assert!(
            has_resumable_context(native.is_some(), recovered),
            "id-gone-AND-marker-gone is the silent-strand shape the transaction rules out"
        );
    }

    #[tokio::test]
    async fn a_worker_switch_lands_tool_clear_and_marker_together() {
        let db = mem().await;
        let (th, dir, sess) = fixture(&db).await;

        repo::switch_worker_engine_txn(&db, dir, sess, "codex", Some("gpt-5.5-high"))
            .await
            .unwrap();

        // BOTH halves of the tool write: `direction.tool` is what
        // `chat_open_worker_impl`'s cold-recreate path reads, `session.tool`
        // what the live path reads.
        assert_eq!(repo::get_direction(&db, dir).await.unwrap().unwrap().tool, "codex");
        let s = repo::get_session(&db, sess).await.unwrap().unwrap();
        assert_eq!(s.tool, "codex");
        assert_eq!(s.model.as_deref(), Some("gpt-5.5-high"));
        let recovered = repo::last_turn_freeze_recovery_secs(&db, th, Some(sess)).await.unwrap();
        assert_eq!(s.native_session_id, None);
        assert!(recovered.is_some());
        assert!(has_resumable_context(s.native_session_id.is_some(), recovered));
    }

    /// The worker's marker is session-scoped. A worker switch must not stamp
    /// the LEAD's grace window and mute its independent re-drive.
    #[tokio::test]
    async fn a_worker_switch_does_not_stamp_the_leads_window() {
        let db = mem().await;
        let (th, dir, sess) = fixture(&db).await;

        repo::switch_worker_engine_txn(&db, dir, sess, "codex", None)
            .await
            .unwrap();

        assert!(repo::last_turn_freeze_recovery_secs(&db, th, Some(sess))
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            repo::last_turn_freeze_recovery_secs(&db, th, None).await.unwrap(),
            None,
            "the lead's own marker is a separate row and must stay unstamped"
        );
    }

    /// A failed switch changes nothing — the transaction is the guarantee, and
    /// the surface stays coherently on its old engine. Driven through #144's
    /// seam on the lead write.
    #[tokio::test]
    async fn a_failed_switch_leaves_the_surface_on_its_old_engine() {
        let db = mem().await;
        let (th, _dir, _sess) = fixture(&db).await;

        let failed = repo::fail_write::while_failing(
            "switch_lead_engine_txn",
            repo::switch_lead_engine_txn(&db, th, "codex", Some("opus")),
        )
        .await;
        assert!(failed.is_err());

        let t = repo::get_thread(&db, th).await.unwrap().unwrap();
        assert_eq!(t.lead_tool, "claude");
        assert_eq!(t.lead_model, None);
        assert_eq!(
            repo::lead_native_id(&db, th).await.unwrap().as_deref(),
            Some("lead-nat-1")
        );
        assert_eq!(
            repo::last_turn_freeze_recovery_secs(&db, th, None).await.unwrap(),
            None,
            "and no marker vouching for a reset that never happened"
        );
    }
}
