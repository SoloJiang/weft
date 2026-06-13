//! Tauri commands for the chat engine. The lead's engine is keyed by
//! `-thread_id`; chat-mode workers (phase 2) key by `session_id`.

use super::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};
use tauri::{AppHandle, Manager, State};

pub(crate) fn lead_key(thread_id: i32) -> i64 {
    -(thread_id as i64)
}

fn ensure_lead_cwd(thread_id: i32) -> anyhow::Result<std::path::PathBuf> {
    let cwd = crate::paths::weft_home()?
        .join("leads")
        .join(thread_id.to_string());
    std::fs::create_dir_all(&cwd)?;
    // git-init so claude's session store (keyed by cwd) behaves like any other
    // cwd; harmless if it already exists.
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status();
    Ok(cwd)
}

/// What a (re)dispatched worker session looks like to the frontend.
#[derive(serde::Serialize, Clone)]
pub struct SessionInfo {
    pub session_id: i32,
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    pub resumed: bool,
    pub native_id: Option<String>,
}

const BASE_PROMPT: &str = "You are the lead for this thread in weft — the human's main collaborator for converging write scope. \
Your mission is to converge the issue's write scope with the human, then propose worker directions. \
Use the weft_planner MCP capabilities when they materially affect scope: read the task when the request is unclear, and read the repo map when repo ownership or cross-repo dependencies matter. \
Do not write code, and do not plan the directions' implementations — each worker decides how to deliver its own direction. \
Ask clarifying questions only when ambiguity changes write scope, acceptance, or sequencing. \
When the write boundary is clear enough for workers to start, call propose_directions with a short rationale and directions \
(name, the ONE repo each writes, reason, mandate). Only list repos each direction must WRITE; reads are free. \
Pick mandate per direction as a planning-depth hint: plan+impl for directions that need worker planning, impl-only for small or fully specified directions. \
Prefer independent directions that can proceed in parallel; put shared contract owners first only when they block others. \
The human reviews and confirms in weft; you can re-propose after more discussion.";

/// Sentinel usage directives appended to the lead prompt. Each subsequent task
/// (Task 3-5) keeps growing this block, so it lives as its own const for easy
/// editing — raw string keeps quotes/JSON readable.
const SENTINEL_DIRECTIVES: &str = r#"When the user has no suitable repo for the work, render a single-line action card by outputting exactly:
<weft:action_card>{"title":"...","body":"...","actions":[{"id":"...","label":"...","kind":"add"|"new"|"clone"}]}</weft:action_card>
Each action's kind must be one of "add" (import existing folder), "new" (create a new repo), or "clone" (clone a remote URL). Use language matching the user's locale for title/body/label. To query the full repo list when the <repo_state> hint is truncated, emit on its own line: <weft:list_repos/> You will receive the reply as <weft:list_repos_result>{...}</weft:list_repos_result>. After a user finishes an action, you will receive <weft:repo_action>{...}</weft:repo_action> with status: ok/error/cancelled."#;

/// The conversational lead prompt. The lead is the human's main collaborator for
/// the thread: it discusses the work, and the plan EMERGES from that conversation
/// rather than from a one-shot propose-and-exit. It proposes when (and only when)
/// the human has converged with it, and may re-propose after more discussion.
pub fn lead_prompt() -> String {
    format!("{BASE_PROMPT}\n\n{SENTINEL_DIRECTIVES}")
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
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(lead_key(thread_id)) {
        return Ok(e);
    }
    let t = repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let cwd = ensure_lead_cwd(thread_id)?;
    let base = app.state::<crate::BusBase>().0.clone();
    let is_concierge = t.kind == "concierge";
    let inj = if is_concierge {
        crate::bus::inject::inject_global(&base, &t.lead_tool, &cwd)
    } else {
        crate::bus::inject::inject_planner(&base, thread_id, &t.lead_tool, &cwd)
    };
    let ask = crate::bus::inject::inject_ask_hook(&base, thread_id, "lead", &t.lead_tool, &cwd);
    crate::skills::inject_for(db, t.workspace_id, &cwd).await;
    let mut extra = ask.args;
    extra.extend(inj.args);
    let system_prompt = if is_concierge {
        concierge_prompt(lang)
    } else {
        let repo_state =
            crate::lead_chat::repo_state::render_repo_state(db, Some(t.workspace_id)).await?;
        format!(
            "{}{}\n\n{}",
            lead_prompt(),
            lang_directive(lang),
            repo_state
        )
    };
    let inner = engine::EngineInner {
        thread_id,
        tool: t.lead_tool.clone(),
        session_id: None,
        cwd,
        extra_args: extra,
        system_prompt,
        native_id: repo::lead_native_id(db, thread_id).await.ok().flatten(),
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, thread_id).await.unwrap_or(1) - 1,
        ask_dir: "lead".into(),
        clock: Default::default(),
        child: None,
        stdin: None,
        current: None,
        interrupting: false,
        generation: 0,
        pending_skill_refresh: false,
        current_origin_tag: None,
    };
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

/// Stop the lead engine (terminal takeover: the session must have one writer).
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
    pub queued: usize,
    pub native_id: Option<String>,
    pub slash_commands: Vec<crate::lead_chat::proto::SlashCmd>,
    pub cwd: String,
}

/// 由「常驻子进程是否存活」与「当前 turn 是否在跑」决定 lead engine 对外报的 state。
/// 纯函数，便于回归测试。
///
/// **busy 优先于 alive**：codex app-server 在共享连接上跑 turn，没有 per-turn 子进程，
/// 故进行中也 alive=false。若先判 alive 会把正在跑的一轮误报成 "stopped"，切页重挂时
/// loadLeadChat 便用它覆盖实时 "busy"，「处理中」占位随之消失。turn 结束/进程死时
/// busy 都会被复位（见 engine.rs on_turn_end / 死亡清理），故 busy 优先是安全的。
fn lead_state_label(alive: bool, busy: bool) -> &'static str {
    if busy {
        "busy"
    } else if !alive {
        "stopped"
    } else {
        "idle"
    }
}

#[cfg(test)]
mod tests {
    use super::{lead_prompt, lead_state_label};

    #[test]
    fn busy_turn_reports_busy_even_without_resident_child() {
        // codex app-server 在共享连接上跑 turn，没有 per-turn 子进程，故进行中
        // alive=false。正在跑的一轮必须仍报 "busy"——否则切页重挂时 loadLeadChat
        // 会用陈旧的 "stopped" 覆盖实时态，「处理中」占位消失。（回归）
        assert_eq!(lead_state_label(false, true), "busy");
        assert_eq!(lead_state_label(true, true), "busy");
        assert_eq!(lead_state_label(true, false), "idle");
        assert_eq!(lead_state_label(false, false), "stopped");
    }

    #[test]
    fn lead_prompt_is_policy_not_fixed_sequence() {
        let prompt = lead_prompt();
        assert!(prompt.contains("converge the issue's write scope"));
        assert!(prompt
            .contains("Use the weft_planner MCP capabilities when they materially affect scope"));
        assert!(!prompt.contains("Start by greeting"));
        assert!(!prompt.contains("call get_task"));
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
}

#[tauri::command]
pub async fn lead_state(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<LeadStateInfo, String> {
    let eng = app.state::<LeadChatState>().get(lead_key(thread_id));
    match eng {
        None => Ok(LeadStateInfo {
            state: "stopped".into(),
            queued: 0,
            native_id: repo::lead_native_id(&db, thread_id).await.ok().flatten(),
            slash_commands: vec![],
            cwd: crate::paths::weft_home()
                .map(|h| {
                    h.join("leads")
                        .join(thread_id.to_string())
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_default(),
        }),
        Some(e) => {
            let mut i = e.lock().await;
            let alive = i
                .child
                .as_mut()
                .map(|c| c.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            Ok(LeadStateInfo {
                state: lead_state_label(alive, i.turn.busy).into(),
                queued: i.turn.queue.len(),
                native_id: i.native_id.clone(),
                slash_commands: i.slash_commands.clone(),
                cwd: i.cwd.to_string_lossy().into_owned(),
            })
        }
    }
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
                crate::opencode::discover_commands(&sess.cwd).await,
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
            let (live, tool, cwd) = {
                let inner = eng.lock().await;
                (
                    inner.slash_commands.clone(),
                    inner.tool.clone(),
                    inner.cwd.clone(),
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
                    let cmds = crate::opencode::discover_commands(&cwd.to_string_lossy()).await;
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
                    merge_local_skill_commands(
                        crate::opencode::discover_commands(&cwd.to_string_lossy()).await,
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
        let guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.values().cloned().collect()
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
    }
    for sid in clean_sessions {
        repo::mark_incomplete_turns_interrupted(&db, thread_id, Some(sid))
            .await
            .map_err(|e| e.to_string())?;
    }
    repo::list_lead_messages(&db, thread_id)
        .await
        .map_err(|e| e.to_string())
}

/// A live worker engine's (direction, repo) slot. The frontend hydrates its
/// session map from these on mount so backend-revived workers (boot recovery,
/// or workers still alive after a frontend reload) get status dots + auto-verify
/// instead of running invisibly.
#[derive(serde::Serialize)]
pub struct LiveWorkerSlot {
    pub direction_id: i32,
    pub repo_id: i32,
    pub thread_id: i32,
}

#[tauri::command]
pub async fn list_live_worker_slots(
    app: AppHandle,
    db: State<'_, Db>,
) -> Result<Vec<LiveWorkerSlot>, String> {
    let engines: Vec<EngineRef> = {
        let state = app.state::<LeadChatState>();
        let guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
        guard.values().cloned().collect()
    };
    let mut out = Vec::new();
    for eng in engines {
        let sid = { eng.lock().await.session_id };
        if let Some(sid) = sid {
            if let Ok(Some(s)) = repo::get_session(&db, sid).await {
                let Ok(Some(dir)) = repo::get_direction(&db, s.direction_id).await else {
                    continue;
                };
                out.push(LiveWorkerSlot {
                    direction_id: s.direction_id,
                    repo_id: s.repo_id,
                    thread_id: dir.thread_id,
                });
            }
        }
    }
    Ok(out)
}

// ───────────────────── chat-mode workers ─────────────────────
//
// Every worker (claude/codex/opencode) runs on the engine: a weft-owned chat
// timeline in the worker conversation surface, with per-tool wire dialects
// (engine::per_turn).
// Each session remains takeover-able in the user's own terminal via its
// native id.

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

pub(crate) async fn chat_open_worker_impl(
    app: &AppHandle,
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    lang: &str,
) -> anyhow::Result<SessionInfo> {
    use sea_orm::EntityTrait;
    let wt = repo::worktree_for(db, direction_id, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no materialized worktree for that direction+repo"))?;
    let dir = crate::store::entities::direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction not found"))?;
    let cwd = std::path::PathBuf::from(&wt.path);

    // Resume an earlier conversation when this slot already captured one.
    let prior = repo::latest_session_for(db, direction_id, repo_id).await?;
    let native = prior.as_ref().and_then(|s| s.native_session_id.clone());
    let resumed = native.is_some();
    let sess = match prior {
        Some(s) if s.native_session_id.is_some() => s,
        _ => repo::create_session(db, direction_id, repo_id, &dir.tool, &wt.path).await?,
    };

    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &dir.tool,
        &cwd,
    );
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &dir.tool,
        &cwd,
    );
    if let Ok(Some(th)) = repo::get_thread(db, dir.thread_id).await {
        crate::skills::inject_for(db, th.workspace_id, &cwd).await;
    }
    let mut extra = ask.args;
    extra.extend(inj.args);

    let state = app.state::<LeadChatState>();
    let key = sess.id as i64;
    let eng = match state.get(key) {
        Some(e) => e,
        None => {
            let inner = engine::EngineInner {
                thread_id: dir.thread_id,
                tool: dir.tool.clone(),
                session_id: Some(sess.id),
                cwd,
                extra_args: extra,
                system_prompt: String::new(),
                native_id: native.clone(),
                slash_commands: vec![],
                turn: Default::default(),
                turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
                ask_dir: direction_id.to_string(),
                clock: Default::default(),
                child: None,
                stdin: None,
                current: None,
                interrupting: false,
                generation: 0,
                pending_skill_refresh: false,
                current_origin_tag: None,
            };
            let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
            state.get_or_insert(key, e)
        }
    };
    engine::ensure_running(app, db, &eng).await?;

    // A fresh conversation starts with a user-shaped task request, followed by
    // the structured Weft brief as context.
    if !resumed {
        let mut brief = crate::brief::assemble(db, direction_id)
            .await
            .unwrap_or_default();
        if !brief.trim().is_empty() {
            brief.push_str(lang_directive(lang));
            engine::send(app, db, &eng, &brief, vec![], vec![], None).await?;
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

    Ok(SessionInfo {
        session_id: sess.id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        resumed,
        native_id: native,
    })
}

/// Get-or-rebuild a worker's engine from its session row — so a chat worker
/// survives app restarts the same way the lead does: sending resumes it.
async fn worker_engine(app: &AppHandle, db: &Db, session_id: i32) -> anyhow::Result<EngineRef> {
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(session_id as i64) {
        return Ok(e);
    }
    use sea_orm::EntityTrait;
    let sess = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let dir = crate::store::entities::direction::Entity::find_by_id(sess.direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction not found"))?;
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
    let inner = engine::EngineInner {
        thread_id: dir.thread_id,
        tool: sess.tool.clone(),
        session_id: Some(sess.id),
        cwd,
        extra_args: extra,
        system_prompt: String::new(),
        native_id: sess.native_session_id.clone(),
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
        ask_dir: sess.direction_id.to_string(),
        clock: Default::default(),
        child: None,
        stdin: None,
        current: None,
        interrupting: false,
        generation: 0,
        pending_skill_refresh: false,
        current_origin_tag: None,
    };
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
/// shoving JSON between in-flight protocol lines. Does NOT ensure_running —
/// a click into a dead lead is a no-op (we don't want a card click to
/// resurrect a stopped engine behind the user's back).
#[tauri::command]
pub async fn post_lead_tool_result(
    app: AppHandle,
    thread_id: i32,
    payload: serde_json::Value,
) -> Result<(), String> {
    let json = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let text = format!("<weft:repo_action>{json}</weft:repo_action>");
    let key = lead_key(thread_id);
    match app.state::<LeadChatState>().get(key) {
        Some(eng) => {
            // TODO: frontend currently can't distinguish delivered vs queued vs
            // no-engine. Acceptable now — action cards are visual + ephemeral
            // — revisit if "card click did nothing" debugging gets noisy.
            let mut inner = eng.lock().await;
            let out = engine::Outgoing {
                text,
                images: vec![],
                tracked: false,
                origin_tag: None,
            };
            if inner.turn.try_begin_send() {
                inner.turn_id += 1;
                inner.clock.begin_turn();
                // Card-click plumbing starts a turn directly (not via send): keep the
                // invariant so a prior concierge tag can't leak onto this turn.
                inner.current_origin_tag = None;
                engine::write_user(&mut inner, &out).await;
            } else {
                inner.turn.queue.push_back(out);
            }
        }
        None => {
            eprintln!("[weft] post_lead_tool_result: no lead engine for thread {thread_id}");
        }
    }
    Ok(())
}
