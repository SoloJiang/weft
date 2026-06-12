//! Tauri commands for the chat engine. The lead's engine is keyed by
//! `-thread_id`; chat-mode workers (phase 2) key by `session_id`.

use super::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};
use tauri::{AppHandle, Manager, State};

fn lead_key(thread_id: i32) -> i64 {
    -(thread_id as i64)
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

const BASE_PROMPT: &str = "You are the lead for this thread in weft — the human's main collaborator. \
Start by greeting briefly and using the weft_planner MCP tools to orient: call get_task to read \
what's being asked, and get_repo_map to learn each repo's role and the cross-repo dependency graph. \
Then DISCUSS the requirement and approach with the human; ask clarifying questions when it matters. \
You do not write code, and you do not plan the directions' implementations — each worker plans its \
own direction. Your job is to converge the scope and ASSIGN ROLES. When you and the human have \
converged on how to split the work, call propose_directions with a short rationale and the directions \
(name, the ONE repo each writes, reason, mandate); only list repos each direction must WRITE \
(reads are free). Pick mandate per direction: plan+impl (default — the worker plans first) or \
impl-only (small/fully-specified — build straight away). The human reviews and confirms in weft; you \
can re-propose after more discussion. Prefer splitting frontend/backend/shared work to run in \
parallel, owner of a shared contract first.";

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
        "你是 weft 桌面端的助理（Concierge），用户从某个飞书会话找你。weft 桌面端正在运行，\
真实状态都在 weft_global MCP 工具里——回答任何关于工作区、issue、待办、agent 提问的问题前，\
必须先用工具核实（list_workspaces / list_issues / pending_needs_you / issue_status），不要凭印象作答。\n\
如果用户消息里带有 feishu_chat_id，那就是当前飞书会话的 chat_id；只有用户语义明确要求为某个已有 issue 创建、打开或继续飞书 topic 时，才可把这个 chat_id 传给 ensure_issue_topic。\n\
\n\
工具一览：\n\
- list_workspaces / list_issues / issue_status / pending_needs_you —— 只读，先用它们摸清状态。\n\
- answer_permission(ask_id, verdict) —— 用户明确告诉你判决时才代答；不确定就先用 pending_needs_you 列出再问用户。\n\
- answer_question(thread_id, ask_id, text) —— 转达用户对 agent 提问的回答。\n\
- message_lead(thread_id, text) —— 把用户的话喂给某个 issue 的 lead。\n\
- ensure_issue_topic(thread_id, chat_id) —— 当用户语义明确要为某个已有 issue 创建/打开/继续飞书 topic 时调用；普通聊天不要调用。\n\
- create_issue(workspace_id, title, kind) —— 新建 issue；kind 默认 feature。\n\
\n\
不要做的事：\n\
- 不要替用户决定需要桌面确认的事（scope 拍板、批准 write trigger、合并保护分支）——把状态报清楚，请用户去桌面处理。\n\
- 不要臆造 issue/工作区/ask 的细节；找不到就说没找到，不要编。\n\
- 不要在不可逆动作之前自行批准权限请求（answer_permission allow/full）——除非用户在这条消息里明确同意。\n\
\n\
回复风格：简短中文，用 markdown 列表/编号；引用 issue 时带 thread_id；引用 ask 时带 ask_id。"
    } else {
        "You are weft's desktop Concierge, reached by the user through one Feishu conversation. weft is \
running on the user's desktop and authoritative state lives behind the `weft_global` MCP \
tools — ALWAYS verify with the tools before answering anything about workspaces, issues, \
pending asks, or agent questions (list_workspaces / list_issues / pending_needs_you / \
issue_status). Never answer from your imagination.\n\
If the user's message includes feishu_chat_id, that is the current Feishu chat_id; only pass it to ensure_issue_topic when the user semantically asks to create, open, or continue a Feishu topic for an existing issue.\n\
\n\
Tools:\n\
- list_workspaces / list_issues / issue_status / pending_needs_you — read-only; lead with these.\n\
- answer_permission(ask_id, verdict) — only when the user explicitly tells you the verdict; otherwise list pending asks and ask.\n\
- answer_question(thread_id, ask_id, text) — relay the user's answer to an agent's open question.\n\
- message_lead(thread_id, text) — deliver the user's message into a specific issue's lead.\n\
- ensure_issue_topic(thread_id, chat_id) — call only when the user semantically asks to create/open/continue a Feishu topic for an existing issue; do not call for ordinary chat.\n\
- create_issue(workspace_id, title, kind) — file a new issue (kind defaults to feature).\n\
\n\
Do not:\n\
- Decide things that require the desktop (scope approval, write-trigger approve/deny, merge-protected branches) — report the state and ask the user to go to the desktop.\n\
- Invent workspace / issue / ask details; if you can't find it, say so.\n\
- Pre-approve irreversible permission asks (answer_permission allow/full) unless the user explicitly consents in this message.\n\
\n\
Style: short, markdown bullets / numbered lists; mention thread_id when citing an issue, ask_id when citing an ask."
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
    use super::lead_state_label;

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
/// `session_id` selects a worker; `thread_id` selects the lead (claude or codex).
#[tauri::command]
pub async fn discover_slash(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: Option<i32>,
    session_id: Option<i32>,
) -> Result<Vec<crate::lead_chat::proto::SlashCmd>, String> {
    let state = app.state::<LeadChatState>();
    if let Some(sid) = session_id {
        let Some(sess) = repo::get_session(&db, sid).await.map_err(|e| e.to_string())? else {
            return Ok(vec![]);
        };
        return Ok(match sess.tool.as_str() {
            "opencode" => crate::opencode::discover_commands(&sess.cwd).await,
            "claude" => match state.get(sid as i64) {
                Some(eng) => eng.lock().await.slash_commands.clone(),
                None => vec![],
            },
            "codex" => crate::codex_slash::discover_commands().await,
            _ => vec![],
        });
    }
    // Lead console: claude carries its own initialize list on the engine;
    // codex (and anything else without a CLI-side list) falls back to the
    // mirrored built-ins so the user still gets a palette.
    if let Some(tid) = thread_id {
        if let Some(eng) = state.get(lead_key(tid)) {
            let inner = eng.lock().await;
            let live = inner.slash_commands.clone();
            if !live.is_empty() {
                return Ok(live);
            }
            if inner.tool == "codex" {
                drop(inner);
                return Ok(crate::codex_slash::discover_commands().await);
            }
        } else if let Ok(Some(t)) = repo::get_thread(&db, tid).await {
            // Lead engine not spawned yet — composer still wants a palette.
            if t.lead_tool == "codex" {
                return Ok(crate::codex_slash::discover_commands().await);
            }
        }
    }
    Ok(vec![])
}

#[tauri::command]
pub async fn list_lead_messages(
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<Vec<crate::store::entities::lead_message::Model>, String> {
    repo::list_lead_messages(&db, thread_id)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────── chat-mode workers ─────────────────────
//
// Every worker (claude/codex/opencode) runs on the engine: a weft-owned chat
// timeline in the SessionView, with per-tool wire dialects (engine::per_turn).
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

async fn chat_open_worker_impl(
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
            };
            let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
            state.get_or_insert(key, e)
        }
    };
    engine::ensure_running(app, db, &eng).await?;

    // A fresh conversation gets its brief as the opening message (the brief is
    // a message, not a system prompt).
    if !resumed {
        let mut brief = crate::brief::assemble(db, direction_id)
            .await
            .unwrap_or_default();
        if !brief.trim().is_empty() {
            brief.push_str(lang_directive(lang));
            engine::send(app, db, &eng, &brief, vec![], vec![]).await?;
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
            };
            if inner.turn.try_begin_send() {
                inner.turn_id += 1;
                inner.clock.begin_turn();
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
