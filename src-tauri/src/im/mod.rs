//! IM 桥（spec: docs/superpowers/specs/2026-06-11-im-feishu-integration-design.md）。
//! 通道无关核心：设置、卡片索引、Channel trait、入站执行、桥运行时。
//! feishu/ 是第一个适配器。结构化动作全走确定性代码，LLM 不在路径上。

pub mod feishu;
pub mod inbound;
pub mod outbound;

use std::collections::HashMap;

pub const K_APP_ID: &str = "im.feishu.app_id";
pub const K_APP_SECRET: &str = "im.feishu.app_secret";
/// 白名单：逗号分隔的飞书 open_id；空 = 未绑定（首个私聊发送者自动绑定）。
pub const K_ALLOW: &str = "im.feishu.allow_open_ids";
/// 启用开关：用户可不删凭证地断开桥。键从未写过时默认「双凭证齐全即开」，
/// 保住升级前「凭证齐全即跑」的老用户不被这次改动断连。
pub const K_ENABLED: &str = "im.feishu.enabled";

#[derive(Clone, Default, PartialEq)]
pub struct ImSettings {
    pub app_id: String,
    pub app_secret: String,
    pub allow_open_ids: Vec<String>,
    /// 用户是否启用了桥（独立于凭证是否齐全）。off = 保留凭证但断开。
    pub enabled: bool,
}

impl std::fmt::Debug for ImSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImSettings")
            .field("app_id", &self.app_id)
            .field(
                "app_secret",
                &if self.app_secret.is_empty() {
                    ""
                } else {
                    "***"
                },
            )
            .field("allow_open_ids", &self.allow_open_ids)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl ImSettings {
    /// 凭证是否齐全（与 enable 开关正交）。桥真正启动还需 `enabled`，见 [`spawn`]。
    pub fn ready(&self) -> bool {
        !self.app_id.is_empty() && !self.app_secret.is_empty()
    }

    pub fn parse_allow(s: &str) -> Vec<String> {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    /// 从 app_setting 读取设置。「键不存在」是默认值；DB 错误原样传播。
    /// Err 必须 fail-closed：桥侧把 Err 当连接错误处理，绝不当作未配置/空白名单
    /// （否则瞬时 DB 错误会清空白名单，导致首个私聊发送者被自动绑定）。
    pub async fn load(db: &crate::store::Db) -> anyhow::Result<Self> {
        use crate::store::repo::get_setting;
        let g = |k: &'static str| async move {
            anyhow::Ok(get_setting(db, k).await?.unwrap_or_default())
        };
        let app_id: String = g(K_APP_ID).await?;
        let app_secret: String = g(K_APP_SECRET).await?;
        let allow_open_ids = Self::parse_allow(&g(K_ALLOW).await?);
        // 键写过就用其值；从未写过则回落到「凭证齐全即开」——保住升级前老用户。
        let has_creds = !app_id.is_empty() && !app_secret.is_empty();
        let enabled = match get_setting(db, K_ENABLED).await? {
            Some(v) => v == "1" || v == "true",
            None => has_creds,
        };
        Ok(Self {
            app_id,
            app_secret,
            allow_open_ids,
            enabled,
        })
    }
}

/// 一张已发出的卡片背后等待的应答目标（回复路由用）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReplyTarget {
    Perm { ask_id: u64 },
    Human { thread: i32, ask_id: u64 },
}

/// 内存卡片索引：出站卡片 message_id ↔ 应答目标（spec §6 内存态）。
#[derive(Default)]
pub struct CardIndex {
    /// ask_id → (message_id, summary)。summary 随卡存档：`AskEvent::Resolved`
    /// 只带 id+answer，patch 终态卡（outbound::resolved_card）要 summary 从这取。
    perm_msg: HashMap<u64, (String, String)>,
    human_msg: HashMap<(i32, u64), String>,
    by_message: HashMap<String, ReplyTarget>,
}

impl CardIndex {
    pub fn record_perm(&mut self, ask_id: u64, message_id: &str, summary: &str) {
        if let Some((old, _)) = self
            .perm_msg
            .insert(ask_id, (message_id.to_string(), summary.to_string()))
        {
            self.by_message.remove(&old);
        }
        self.by_message
            .insert(message_id.to_string(), ReplyTarget::Perm { ask_id });
    }
    pub fn record_human(&mut self, thread: i32, ask_id: u64, message_id: &str) {
        if let Some(old) = self
            .human_msg
            .insert((thread, ask_id), message_id.to_string())
        {
            self.by_message.remove(&old);
        }
        self.by_message.insert(
            message_id.to_string(),
            ReplyTarget::Human { thread, ask_id },
        );
    }
    pub fn target_of(&self, message_id: &str) -> Option<ReplyTarget> {
        self.by_message.get(message_id).copied()
    }
    /// 解决后取走（patch 终态用），并清反向索引。返回 (message_id, summary)。
    pub fn take_perm(&mut self, ask_id: u64) -> Option<(String, String)> {
        let (m, s) = self.perm_msg.remove(&ask_id)?;
        self.by_message.remove(&m);
        Some((m, s))
    }
    pub fn take_human(&mut self, thread: i32, ask_id: u64) -> Option<String> {
        let m = self.human_msg.remove(&(thread, ask_id))?;
        self.by_message.remove(&m);
        Some(m)
    }
}

/// IM 通道抽象（spec §2.1）：当前提供飞书实现 + 测试替身；第二通道出现时
/// 在这里复用 owner、卡片索引、路由执行与回流语义。
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// 发交互卡片到用户（p2p），返回 message_id。
    async fn send_card(&self, open_id: &str, card: serde_json::Value) -> anyhow::Result<String>;
    /// 把已发卡片 patch 成终态。
    async fn patch_card(&self, message_id: &str, card: serde_json::Value) -> anyhow::Result<()>;
    /// 发纯文本到用户（p2p）。
    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()>;
    /// 发纯文本到群聊，返回根 message_id；用作 issue topic 的锚点。
    async fn send_chat_text(&self, _chat_id: &str, _text: &str) -> anyhow::Result<String> {
        anyhow::bail!("send_chat_text unsupported by this channel")
    }
    /// 回复一条已存在的消息（M2-4：lead 回流飞书话题）。reply_to 必须是话题
    /// 根消息或话题内任意一条消息——飞书 `reply` API 自动把回复挂到同一话题。
    /// 返回新发消息的 message_id（供后续 reaction 之类的回执使用）。
    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String>;
    /// 给指定消息加一个 emoji 表情回执（M2-6：入站收到 → 👀）。返回 reaction_id
    /// 用于稍后 delete；通道不支持 reaction 时默认实现返回空串（调用方应据此跳过）。
    async fn add_reaction(&self, _message_id: &str, _emoji: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    /// 删除之前加上的 reaction（M2-6：首次出站前清掉 👀）。
    async fn delete_reaction(&self, _message_id: &str, _reaction_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// M2-6 桥运行时上下文：让 execute() 在入站 IssueMessage 路径里挂 👀，
/// 同时把 (im_message_id, reaction_id) 记到 `acks[thread_id]`——lead 首条
/// 出站时 [`spawn`] 出站任务取走清空。`message_id`/`acks` 任一缺失即跳过
/// reaction（测试路径 / 配置未注入 都安全）。
#[derive(Default)]
pub struct ExecuteCtx {
    pub inbound_message_id: Option<String>,
    pub acks: Option<Arc<tokio::sync::Mutex<HashMap<i32, Vec<(String, String)>>>>>,
}

/// Route execution requires an AppHandle when an issue message has to be fed
/// into the lead engine (M2-3 / M3 Concierge): the engine wiring (planner MCP,
/// ask hook, etc.) lives on app state. For tests that don't exercise those
/// paths, pass None — IssueMessage / FreeText that needs the app degrade to
/// a polite stub instead of panicking.
///
/// `ctx`（M2-6）：桥运行时塞进的额外上下文——目前只有「这条入站消息的飞书
/// message_id」用于挂 👀 reaction。tests 传 None 即可。
pub async fn execute(
    route: inbound::Route,
    db: &crate::store::Db,
    asks: &crate::ask::AskRegistry,
    bus: &crate::bus::BusRegistry,
    channel: &dyn Channel,
    sender: &str,
    lang: &str,
    app: Option<&tauri::AppHandle>,
    ctx: Option<&ExecuteCtx>,
) -> anyhow::Result<()> {
    let t = |zh: &'static str, en: &'static str| if lang == "zh" { zh } else { en };
    match route {
        inbound::Route::Ignore => {}
        inbound::Route::Bind {
            open_id,
            chat_id,
            text,
        } => {
            // Route 读的是 allow 快照；落库前重查仍为空（Route::Bind doc 的竞态契约）。
            let cur = crate::store::repo::get_setting(db, K_ALLOW)
                .await?
                .unwrap_or_default();
            if !ImSettings::parse_allow(&cur).is_empty() {
                return Ok(()); // 已有 owner：本次绑定静默放弃
            }
            crate::store::repo::set_setting(db, K_ALLOW, &open_id).await?;
            if let Err(e) = channel
                .send_text(
                    &open_id,
                    t(
                        "绑定成功 ✓ 之后 Weft 的权限请求和 agent 提问会推送到这里，回复卡片消息即可作答。",
                        "Bound ✓ Weft will push permission asks and agent questions here; reply to a card to answer.",
                    ),
                )
                .await
            {
                eprintln!("[weft][im] bind confirm: {e}");
            }
            if let Some(app) = app {
                if !text.trim().is_empty() {
                    let im_thread_ref = format!("dm:{open_id}");
                    if let Err(e) = consume_free_text(
                        app,
                        db,
                        &open_id,
                        &chat_id,
                        &im_thread_ref,
                        None,
                        &text,
                        lang,
                    )
                    .await
                    {
                        eprintln!("[weft][im] concierge after bind: {e}");
                    }
                }
            }
        }
        inbound::Route::BindIssueThread {
            thread_id,
            chat_id,
            im_thread_ref,
        } => {
            let thread = crate::store::repo::get_thread(db, thread_id).await?;
            let Some(thread) = thread else {
                if let Err(e) = channel
                    .send_text(
                        sender,
                        &t("没有找到这个 issue。", "No issue with that id was found."),
                    )
                    .await
                {
                    eprintln!("[weft][im] bind-issue missing hint: {e}");
                }
                return Ok(());
            };
            crate::store::repo::bind_im_route(db, thread_id, "feishu", &chat_id, &im_thread_ref)
                .await?;
            if let Err(e) = channel
                .send_text(
                    sender,
                    &format!(
                        "{} #{} · {}",
                        t("已绑定飞书话题到", "Bound this Feishu topic to"),
                        thread.id,
                        thread.title
                    ),
                )
                .await
            {
                eprintln!("[weft][im] bind-issue confirm: {e}");
            }
        }
        inbound::Route::EnsureIssueTopic {
            thread_id,
            chat_id,
            reply_to,
        } => {
            ensure_issue_topic(db, channel, thread_id, &chat_id, Some(&reply_to), lang).await?;
        }
        inbound::Route::AnswerPerm { ask_id, answer } => {
            if !asks.answer(ask_id, answer) {
                if let Err(e) = channel
                    .send_text(
                        sender,
                        t(
                            "这条权限请求已被处理或已过期。",
                            "That permission ask was already handled or has expired.",
                        ),
                    )
                    .await
                {
                    eprintln!("[weft][im] stale-perm hint: {e}");
                }
            }
            // 终态卡 patch 由桥的 AskEvent::Resolved 消费侧统一做（双面同源）。
        }
        inbound::Route::AnswerHuman {
            thread,
            ask_id,
            text,
        } => {
            if !bus.answer_ask(thread, ask_id, &text) {
                if let Err(e) = channel
                    .send_text(
                        sender,
                        t(
                            "这个提问已被回答过了。",
                            "That question was already answered.",
                        ),
                    )
                    .await
                {
                    eprintln!("[weft][im] stale-human hint: {e}");
                }
            }
        }
        inbound::Route::BadVerdict => {
            if let Err(e) = channel
                .send_text(
                    sender,
                    t(
                        "没看懂。回复：允许 / 拒绝 / 总是 / 放行（或 1/2/3/4）。",
                        "Didn't catch that. Reply: allow / deny / always / full (or 1/2/3/4).",
                    ),
                )
                .await
            {
                eprintln!("[weft][im] verdict hint: {e}");
            }
        }
        inbound::Route::FreeText {
            sender_open_id,
            chat_id,
            im_thread_ref,
            reply_to,
            text,
        } => {
            // 每个 IM 会话独立 Concierge：同一个飞书私聊/群聊复用自己的
            // concierge thread，不把不同 IM 上下文混进全局单例。
            let _ = (&sender_open_id, &chat_id, &im_thread_ref, &reply_to, &text);
            if let Some(app) = app {
                if let Err(e) =
                    consume_free_text(app, db, &sender_open_id, &chat_id, &im_thread_ref, reply_to.as_deref(), &text, lang).await
                {
                    eprintln!("[weft][im] concierge: {e}");
                }
            } else if let Err(e) = channel
                .send_text(
                    sender,
                    t(
                        "自由对话（当前 IM 会话助理）需要桌面 app 运行上下文；当前路径无法处理，请回复卡片消息作答权限与提问。",
                        "Free chat (this IM conversation's concierge) needs the desktop app context; this path cannot handle it, so reply to cards for asks.",
                    ),
                )
                .await
            {
                eprintln!("[weft][im] freetext hint: {e}");
            }
        }
        inbound::Route::IssueMessage {
            chat_id,
            im_thread_ref,
            sender_open_id: _,
            text,
        } => {
            // 飞书话题/群会话里的消息 → 反查 im_route 命中 issue → 灌进 lead engine。
            // 未绑定不自动创建 issue；issue 是主对象，topic 通过 `/topic <issue-id>`
            // 或桌面绑定动作创建/绑定。
            let r =
                crate::store::repo::im_route_of_thread_ref(db, "feishu", &chat_id, &im_thread_ref)
                    .await?;
            let Some(route) = r else {
                if let Some(ctx) = ctx {
                    if let Some(mid) = ctx.inbound_message_id.as_deref() {
                        if let Err(e) = channel
                            .reply_text(
                                mid,
                                "这段飞书话题还没有绑定 Weft issue。发送 /bind <issue-id> 绑定当前话题，或在群里发送 /topic <issue-id> 创建 issue topic。",
                            )
                            .await
                        {
                            eprintln!("[weft][im] unbound topic hint: {e}");
                        }
                    }
                }
                return Ok(());
            };
            // M2-6 回执：在投递 engine 之前先挂 👀——出站前批量 delete。
            // ctx 没给 message_id / acks 则跳过；reaction add 失败不阻挡后续灌入。
            if let (Some(ctx), true) = (
                ctx,
                ctx.map(|c| c.inbound_message_id.is_some()).unwrap_or(false),
            ) {
                if let (Some(mid), Some(acks)) =
                    (ctx.inbound_message_id.as_deref(), ctx.acks.as_ref())
                {
                    match channel.add_reaction(mid, "EYES").await {
                        Ok(rid) => {
                            acks.lock()
                                .await
                                .entry(route.thread_id)
                                .or_default()
                                .push((mid.to_string(), rid));
                        }
                        Err(e) => eprintln!("[weft][im] add reaction: {e}"),
                    }
                }
            }
            let Some(app) = app else { return Ok(()) }; // 测试路径不进 engine
            if let Err(e) = feed_issue_message(app, db, route.thread_id, &text, lang).await {
                eprintln!("[weft][im] issue lead send: {e}");
            }
        }
    }
    Ok(())
}

// ───────────────────────── 桥运行时（Task 10）─────────────────────────

use std::sync::Arc;
use tauri::Manager;

/// IM 出站文案默认语言。后端无持久化 UI 语言设置（lang 是 lead/worker 的
/// 逐命令入参），桥侧固定中文优先（项目主语言）。
const IM_LANG: &str = "zh";

/// 桥的共享态：代际号杀旧任务（设置变更/重连后旧 spawn 自然退出）；状态串供
/// Settings 显示；卡片索引跨出站/入站任务共享。
#[derive(Default)]
pub struct ImBridge {
    inner: Arc<std::sync::Mutex<BridgeInner>>,
}

#[derive(Default)]
struct BridgeInner {
    generation: u64,
    /// "disabled" | "connecting" | "online" | "error: …"
    status: String,
    cards: Arc<tokio::sync::Mutex<CardIndex>>,
    /// M2-6: 入站 👀 reaction 簿记。键 = lead_chat thread_id；值 = 这次 lead
    /// 出站前应当 delete 的 (im_message_id, reaction_id) 列表。lead 一旦
    /// finalize 出站，桥侧把对应 thread 的所有挂账 reaction 全部清掉——队列
    /// 里挤压的多条 👀 一次性收回，回执语义诚实反映「轮到这条被回复」。
    pending_acks: Arc<tokio::sync::Mutex<HashMap<i32, Vec<(String, String)>>>>,
}

impl ImBridge {
    pub fn status(&self) -> String {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.status.is_empty() {
            "disabled".to_string()
        } else {
            g.status.clone()
        }
    }
    fn set_status(&self, s: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status = s.to_string();
    }
    /// 起新一代：自增代际号、换一张干净的卡片索引（旧任务下次 live() 检查时退出）。
    fn bump(
        &self,
    ) -> (
        u64,
        Arc<tokio::sync::Mutex<CardIndex>>,
        Arc<tokio::sync::Mutex<HashMap<i32, Vec<(String, String)>>>>,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.generation += 1;
        g.cards = Arc::new(tokio::sync::Mutex::new(CardIndex::default()));
        g.pending_acks = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        (g.generation, g.cards.clone(), g.pending_acks.clone())
    }
    fn live(&self, generation: u64) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
            == generation
    }
}

/// 启动（或重启）桥：读设置→不 ready 则置 disabled；ready 则装通知器、起出站
/// 消费与 ws 入站两个任务。设置变更后再次调用即可（代际号淘汰旧任务）。
/// 通知器在「不 ready 提前返回」前不安装——避免 disabled 时仍堆积事件。
pub fn spawn(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let bridge = app.state::<ImBridge>();
        let (generation, cards, acks) = bridge.bump();
        let db = app.state::<crate::store::Db>().inner().clone();

        let settings = match ImSettings::load(&db).await {
            Ok(s) => s,
            Err(e) => {
                // fail-closed：DB/连接错误不当作未配置，置 error 并退出本代。
                bridge.set_status(&format!("error: {e}"));
                eprintln!("[weft][im] load settings: {e}");
                return;
            }
        };
        // 启动需「已启用 且 凭证齐全」。关开关 = 保留凭证但断开（status 回 disabled，
        // 旧代任务下次 live() 检查时退出）。
        if !(settings.enabled && settings.ready()) {
            bridge.set_status("disabled");
            return;
        }
        bridge.set_status("connecting");

        let channel: Arc<dyn Channel> = Arc::new(feishu::FeishuChannel::new(
            &settings.app_id,
            &settings.app_secret,
        ));

        // —— 出站：registry 通知 → 发卡/patch ——
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel();
        let (hum_tx, mut hum_rx) = tokio::sync::mpsc::unbounded_channel();
        // set_notifier 返回挂接瞬间已 open 的快照：桥重启时补发卡片（无 miss/dup）。
        let snapshot = app.state::<crate::ask::AskRegistry>().set_notifier(ask_tx);
        app.state::<crate::bus::BusRegistry>()
            .set_ask_notifier(hum_tx);
        {
            let (app2, db2, ch, cards2) = (app.clone(), db.clone(), channel.clone(), cards.clone());
            tauri::async_runtime::spawn(async move {
                let bridge = app2.state::<ImBridge>();
                // 先补发快照里的已开 Ask（挂接前就 open 的，不会再有 Opened 事件）。
                for ask in snapshot {
                    if !bridge.live(generation) {
                        return;
                    }
                    consume_ask_event(
                        crate::ask::AskEvent::Opened(ask),
                        &db2,
                        ch.as_ref(),
                        &cards2,
                    )
                    .await;
                }
                loop {
                    if !bridge.live(generation) {
                        return;
                    }
                    tokio::select! {
                        ev = ask_rx.recv() => match ev {
                            None => return,
                            Some(ev) => consume_ask_event(ev, &db2, ch.as_ref(), &cards2).await,
                        },
                        ev = hum_rx.recv() => match ev {
                            None => return,
                            Some(ev) => consume_human_event(ev, &db2, ch.as_ref(), &cards2).await,
                        },
                    }
                }
            });
        }

        // —— 入站：ws → 路由 → 执行 ——
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let (app2, db2, ch, cards2, acks2) = (
                app.clone(),
                db.clone(),
                channel.clone(),
                cards.clone(),
                acks.clone(),
            );
            tauri::async_runtime::spawn(async move {
                let bridge = app2.state::<ImBridge>();
                while let Some(inb) = in_rx.recv().await {
                    if !bridge.live(generation) {
                        return;
                    }
                    // 每条入站重读白名单（绑定后即时生效）；Err 丢弃该条（fail-closed）。
                    let allow = match ImSettings::load(&db2).await {
                        Ok(s) => s.allow_open_ids,
                        Err(e) => {
                            eprintln!("[weft][im] reload allowlist: {e}");
                            continue;
                        }
                    };
                    let (sender, in_mid) = match &inb {
                        inbound::Inbound::Text {
                            sender_open_id,
                            message_id,
                            ..
                        } => (sender_open_id.clone(), Some(message_id.clone())),
                        inbound::Inbound::Action {
                            operator_open_id, ..
                        } => (operator_open_id.clone(), None),
                    };
                    let r = { inbound::route(&inb, &allow, &*cards2.lock().await) };
                    let route_name = match &r {
                        inbound::Route::Ignore => "ignore",
                        inbound::Route::Bind { .. } => "bind",
                        inbound::Route::BindIssueThread { .. } => "bind_issue_thread",
                        inbound::Route::EnsureIssueTopic { .. } => "ensure_issue_topic",
                        inbound::Route::AnswerPerm { .. } => "answer_perm",
                        inbound::Route::AnswerHuman { .. } => "answer_human",
                        inbound::Route::BadVerdict => "bad_verdict",
                        inbound::Route::IssueMessage { .. } => "issue_message",
                        inbound::Route::FreeText { .. } => "free_text",
                    };
                    eprintln!("[weft][im] route={route_name} sender={sender}");
                    let asks = app2.state::<crate::ask::AskRegistry>();
                    let bus = app2.state::<crate::bus::BusRegistry>();
                    let ctx = ExecuteCtx {
                        inbound_message_id: in_mid,
                        acks: Some(acks2.clone()),
                    };
                    if let Err(e) = execute(
                        r,
                        &db2,
                        &asks,
                        &bus,
                        ch.as_ref(),
                        &sender,
                        IM_LANG,
                        Some(&app2),
                        Some(&ctx),
                    )
                    .await
                    {
                        eprintln!("[weft][im] execute: {e}");
                    }
                }
            });
        }

        // —— 回流：lead engine assistant 文本 finalize → 反查 im_route → 飞书 reply ——
        // 没注册 LeadOutHub（单测可能这样跑）则跳过——桥也能正常处理入站。
        if let Some(hub) = app.try_state::<crate::lead_chat::out_hub::LeadOutHub>() {
            let mut rx = hub.subscribe();
            let (db2, ch, acks2) = (db.clone(), channel.clone(), acks.clone());
            let app4 = app.clone();
            tauri::async_runtime::spawn(async move {
                let bridge = app4.state::<ImBridge>();
                loop {
                    if !bridge.live(generation) {
                        return;
                    }
                    match rx.recv().await {
                        Ok(out) => {
                            consume_lead_out(out, &db2, ch.as_ref(), &acks2).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // engine 产文本太快 / 桥太慢——容量 64 已远超单轮 finalize
                            // 量级，跑到这里多半是死锁前兆，只丢日志不退出。
                            eprintln!("[weft][im] lead-out lagged: {n} dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
        }

        // —— ws 长连接（断线指数退避重连） ——
        // open-lark 的 EventDispatcherHandler 含 Box<dyn EventHandler>（无 Send
        // 约束），LarkWsClient::open 的 future 因此 !Send，过不了 Tauri 的
        // async_runtime::spawn（要求 Send）。故起一条独立 OS 线程跑 current-thread
        // 运行时——!Send future 在 block_on 下合法。跨线程的只有 in_tx / 凭证串 /
        // AppHandle（都是 Send）；!Send 的 handler 全程留在该线程。
        let (app_id, app_secret) = (settings.app_id.clone(), settings.app_secret.clone());
        let app3 = app.clone();
        let ch_for_summary = channel.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[weft][im] ws runtime: {e}");
                    app3.state::<ImBridge>().set_status(&format!("error: {e}"));
                    return;
                }
            };
            rt.block_on(async move {
                let bridge = app3.state::<ImBridge>();
                let mut backoff = 1u64;
                // M3-4：本代际只发一次「上线摘要」。bump() 起新代际时本变量随
                // 闭包一起被新线程重建，于是重启桥（设置变化/凭证更新）会自然
                // 再播一次；ws 重连（同代际内 sleep+retry）不重发。
                let mut sent_resync = false;
                loop {
                    if !bridge.live(generation) {
                        return;
                    }
                    bridge.set_status("online"); // 连接建立细节在 run_ws 内
                    if !sent_resync {
                        send_resync_summary(&app3, ch_for_summary.as_ref()).await;
                        sent_resync = true;
                    }
                    match feishu::ws::run_ws(app_id.clone(), app_secret.clone(), in_tx.clone())
                        .await
                    {
                        Ok(()) => backoff = 1,
                        Err(e) => {
                            bridge.set_status(&format!("error: {e}"));
                            eprintln!("[weft][im] ws: {e}");
                        }
                    }
                    if !bridge.live(generation) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            });
        });
    });
}

/// 权限 Ask 事件 → 发卡（Opened，查 DB 富化 thread 标题/direction 名）/
/// patch 终态（Resolved 带真实判决；Cancelled = 过期回落）。未绑定不出站。
async fn consume_ask_event(
    ev: crate::ask::AskEvent,
    db: &crate::store::Db,
    ch: &dyn Channel,
    cards: &tokio::sync::Mutex<CardIndex>,
) {
    let owner = match ImSettings::load(db).await {
        Ok(s) => s.allow_open_ids.into_iter().next(),
        Err(e) => {
            eprintln!("[weft][im] consume_ask load owner: {e}");
            return;
        }
    };
    let Some(owner) = owner else { return }; // 未绑定不出站
    match ev {
        crate::ask::AskEvent::Opened(mut a) => {
            if let Ok(Some(t)) = crate::store::repo::get_thread(db, a.thread).await {
                a.thread_title = t.title;
            }
            if let Ok(id) = a.dir.parse::<i32>() {
                if let Ok(Some(d)) = crate::store::repo::get_direction(db, id).await {
                    a.dir_name = d.name;
                }
            }
            let summary = a.summary.clone();
            match ch.send_card(&owner, outbound::perm_card(&a, IM_LANG)).await {
                Ok(mid) => cards.lock().await.record_perm(a.id, &mid, &summary),
                Err(e) => eprintln!("[weft][im] send perm card: {e}"),
            }
        }
        crate::ask::AskEvent::Resolved { id, answer } => {
            if let Some((mid, summary)) = cards.lock().await.take_perm(id) {
                let card = outbound::resolved_card(&summary, answer.as_str(), IM_LANG);
                if let Err(e) = ch.patch_card(&mid, card).await {
                    eprintln!("[weft][im] patch resolved card: {e}");
                }
            }
        }
        crate::ask::AskEvent::Cancelled { id } => {
            if let Some((mid, summary)) = cards.lock().await.take_perm(id) {
                let card = outbound::resolved_card(&summary, "cancelled", IM_LANG);
                if let Err(e) = ch.patch_card(&mid, card).await {
                    eprintln!("[weft][im] patch cancelled card: {e}");
                }
            }
        }
    }
}

/// ask_human 事件 → 发提问卡（查 DB 富化 thread 标题/提问 direction 名）/
/// patch 已答终态（带人答文本）。未绑定不出站。
async fn consume_human_event(
    ev: crate::bus::state::HumanAskEvent,
    db: &crate::store::Db,
    ch: &dyn Channel,
    cards: &tokio::sync::Mutex<CardIndex>,
) {
    let owner = match ImSettings::load(db).await {
        Ok(s) => s.allow_open_ids.into_iter().next(),
        Err(e) => {
            eprintln!("[weft][im] consume_human load owner: {e}");
            return;
        }
    };
    let Some(owner) = owner else { return };
    match ev {
        crate::bus::state::HumanAskEvent::Asked { thread, ask } => {
            let title = crate::store::repo::get_thread(db, thread)
                .await
                .ok()
                .flatten()
                .map(|t| t.title)
                .unwrap_or_default();
            let from = match ask.from.parse::<i32>() {
                Ok(d) => crate::store::repo::get_direction(db, d)
                    .await
                    .ok()
                    .flatten()
                    .map(|d| d.name)
                    .unwrap_or_else(|| ask.from.clone()),
                Err(_) => ask.from.clone(),
            };
            match ch
                .send_card(
                    &owner,
                    outbound::human_card(&title, &from, &ask.text, IM_LANG),
                )
                .await
            {
                Ok(mid) => cards.lock().await.record_human(thread, ask.id, &mid),
                Err(e) => eprintln!("[weft][im] send human card: {e}"),
            }
        }
        crate::bus::state::HumanAskEvent::Answered {
            thread,
            ask_id,
            text,
        } => {
            if let Some(mid) = cards.lock().await.take_human(thread, ask_id) {
                let card = outbound::human_resolved_card(&text, IM_LANG);
                if let Err(e) = ch.patch_card(&mid, card).await {
                    eprintln!("[weft][im] patch human resolved card: {e}");
                }
            }
        }
    }
}

/// M2-3: 把飞书话题里的一条消息灌进 issue 对应的 lead engine。
/// 不感知前端 lang 设置——桥侧固定中文（spec：IM 出站默认 zh）。
async fn feed_issue_message(
    app: &tauri::AppHandle,
    db: &crate::store::Db,
    thread_id: i32,
    text: &str,
    lang: &str,
) -> anyhow::Result<()> {
    let eng = crate::lead_chat::commands::lead_engine(app, db, thread_id, lang).await?;
    crate::lead_chat::engine::send(app, db, &eng, text, Vec::new(), Vec::new()).await
}

pub async fn ensure_issue_topic(
    db: &crate::store::Db,
    ch: &dyn Channel,
    thread_id: i32,
    chat_id: &str,
    reply_to: Option<&str>,
    lang: &str,
) -> anyhow::Result<()> {
    let Some(thread) = crate::store::repo::get_thread(db, thread_id).await? else {
        if let Some(reply_to) = reply_to {
            if let Err(e) = ch
                .reply_text(
                    reply_to,
                    if lang == "zh" {
                        "没有找到这个 issue。"
                    } else {
                        "No issue with that id was found."
                    },
                )
                .await
            {
                eprintln!("[weft][im] ensure-topic missing issue: {e}");
            }
        }
        return Ok(());
    };

    if let Some(route) = crate::store::repo::im_route_of_thread(db, thread_id).await? {
        if let Some(reply_to) = reply_to {
            if let Err(e) = ch
                .reply_text(
                    reply_to,
                    &format!(
                        "{} #{} · {}",
                        if lang == "zh" {
                            "这个 issue 已有飞书 topic"
                        } else {
                            "This issue already has a Feishu topic"
                        },
                        thread.id,
                        thread.title
                    ),
                )
                .await
            {
                eprintln!("[weft][im] ensure-topic existing hint: {e}");
            }
        }
        // route 保持不变：后续 lead 输出仍会进入已有 topic。
        let _ = route;
        return Ok(());
    }

    let root = ch
        .send_chat_text(
            chat_id,
            &format!(
                "Weft issue #{} · {}\n这个飞书话题已绑定到 Weft issue。",
                thread.id, thread.title
            ),
        )
        .await?;
    crate::store::repo::bind_im_route(db, thread.id, "feishu", chat_id, &root).await?;
    if let Some(reply_to) = reply_to {
        if let Err(e) = ch
            .reply_text(
                reply_to,
                &format!(
                    "{} #{} · {}",
                    if lang == "zh" {
                        "已创建并绑定飞书 topic 到"
                    } else {
                        "Created and bound a Feishu topic to"
                    },
                    thread.id,
                    thread.title
                ),
            )
            .await
        {
            eprintln!("[weft][im] ensure-topic created hint: {e}");
        }
    }
    Ok(())
}

/// M2-4: lead engine 的 assistant 文本完成 → 反查 im_route → 飞书话题 reply。
/// 同时把这个 thread 挂账的 👀 reactions 一次性 delete（spec §4 回执语义：
/// 「轮到这条被回复」才取下 👀，排队期间一直在）。pub 给集成测试用。
pub async fn consume_lead_out(
    out: crate::lead_chat::out_hub::LeadOut,
    db: &crate::store::Db,
    ch: &dyn Channel,
    acks: &Arc<tokio::sync::Mutex<HashMap<i32, Vec<(String, String)>>>>,
) {
    // 反查 im_route：普通 issue 通过绑定话题回飞书；Concierge 通过
    // feishu_concierge route 回到发起它的 IM 会话。
    let route = match crate::store::repo::im_route_of_thread(db, out.thread_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            eprintln!("[weft][im] lead-out lookup route: {e}");
            return;
        }
    };
    if route.channel == "feishu_concierge" {
        let send = if let Some(open_id) = route.im_thread_ref.strip_prefix("dm:") {
            ch.send_text(open_id, &out.text).await.map(|_| ())
        } else if route.im_thread_ref.starts_with("chat:") {
            ch.send_chat_text(&route.chat_id, &out.text)
                .await
                .map(|_| ())
        } else {
            Err(anyhow::anyhow!(
                "unknown concierge im_thread_ref {}",
                route.im_thread_ref
            ))
        };
        if let Err(e) = send {
            eprintln!("[weft][im] concierge reply: {e}");
        }
        return;
    }
    let body = outbound::issue_reply_text(IM_LANG, &out.text);
    // im_thread_ref 即话题根 message_id：飞书 reply API 会把回复挂同一话题。
    if let Err(e) = ch.reply_text(&route.im_thread_ref, &body).await {
        eprintln!("[weft][im] reply lead text: {e}");
        return; // reply 失败就不 clear 回执——下一条 lead 还会带它走。
    }
    // 出站成功 → 清掉这个 thread 上挂的所有 👀。
    let pending: Vec<(String, String)> = {
        let mut g = acks.lock().await;
        g.remove(&out.thread_id).unwrap_or_default()
    };
    for (mid, rid) in pending {
        if let Err(e) = ch.delete_reaction(&mid, &rid).await {
            eprintln!("[weft][im] delete reaction: {e}");
        }
    }
}

async fn ensure_concierge_workspace(db: &crate::store::Db) -> anyhow::Result<i32> {
    if let Some(id) = crate::store::repo::get_setting(db, crate::store::repo::K_CONCIERGE_WORKSPACE)
        .await?
        .and_then(|s| s.parse::<i32>().ok())
    {
        if crate::store::repo::list_workspaces(db)
            .await?
            .into_iter()
            .any(|ws| ws.id == id)
        {
            return Ok(id);
        }
    }

    let ws = crate::store::repo::create_workspace(db, "Concierge").await?;
    crate::store::repo::set_setting(
        db,
        crate::store::repo::K_CONCIERGE_WORKSPACE,
        &ws.id.to_string(),
    )
    .await?;
    Ok(ws.id)
}

async fn ensure_im_concierge_thread(
    db: &crate::store::Db,
    sender_open_id: &str,
    chat_id: &str,
    im_thread_ref: &str,
) -> anyhow::Result<i32> {
    if let Some(route) =
        crate::store::repo::im_route_of_thread_ref(db, "feishu_concierge", chat_id, im_thread_ref)
            .await?
    {
        if crate::store::repo::get_thread(db, route.thread_id)
            .await?
            .is_some()
        {
            return Ok(route.thread_id);
        }
        crate::store::repo::unbind_im_route(db, route.thread_id).await?;
    }

    let ws_id = ensure_concierge_workspace(db).await?;
    let title = if im_thread_ref.starts_with("dm:") {
        format!("飞书私聊 · {sender_open_id}")
    } else {
        format!("飞书群聊 · {chat_id}")
    };
    let tool = crate::tools::default_tool(db).await;
    let thread = crate::store::repo::create_thread(db, ws_id, &title, "concierge", &tool).await?;
    crate::store::repo::bind_im_route(db, thread.id, "feishu_concierge", chat_id, im_thread_ref)
        .await?;
    Ok(thread.id)
}

/// M3-3: IM 自由文本 → 该 IM 会话独立的 Concierge engine。
async fn consume_free_text(
    app: &tauri::AppHandle,
    db: &crate::store::Db,
    sender_open_id: &str,
    chat_id: &str,
    im_thread_ref: &str,
    reply_to: Option<&str>,
    text: &str,
    lang: &str,
) -> anyhow::Result<()> {
    let thread_id = ensure_im_concierge_thread(db, sender_open_id, chat_id, im_thread_ref).await?;
    let eng = crate::lead_chat::commands::lead_engine(app, db, thread_id, lang).await?;
    let framed = match reply_to {
        Some(mid) => format!(
            "[from {sender_open_id}; feishu_chat_id={chat_id}; feishu_message_id={mid}] {text}"
        ),
        None => format!("[from {sender_open_id}; feishu_chat_id={chat_id}] {text}"),
    };
    crate::lead_chat::engine::send(app, db, &eng, &framed, Vec::new(), Vec::new()).await
}

/// M3-4: 桥上线后向 owner 私聊推一次「待办摘要」。整段 best-effort：任一
/// 步骤出错都只 log——失败不能阻挡 ws 入站消费（spec §4「上线即可用」）。
/// 未绑定 owner / 无待办 / channel 发送失败都 silent-skip。
async fn send_resync_summary(app: &tauri::AppHandle, ch: &dyn Channel) {
    let db = app.state::<crate::store::Db>().inner().clone();
    let asks = app.state::<crate::ask::AskRegistry>();
    let owner = match ImSettings::load(&db).await {
        Ok(s) => s.allow_open_ids.into_iter().next(),
        Err(e) => {
            eprintln!("[weft][im] resync load owner: {e}");
            return;
        }
    };
    let Some(owner) = owner else { return };
    let items = build_resync_items(&db, asks.inner()).await;
    let body = outbound::resync_summary(IM_LANG, &items);
    if body.is_empty() {
        return; // 无积压：spec 明确「上线时无待办则不打扰」
    }
    if let Err(e) = ch.send_text(&owner, &body).await {
        eprintln!("[weft][im] resync send: {e}");
    }
}

/// 把 AskRegistry 当前快照拉成 `(thread_id, "标题：summary")` 列表供
/// [`outbound::resync_summary`] 渲染。pub(super) 仅为单测可见；正式调用
/// 入口是 [`send_resync_summary`]。
pub(crate) async fn build_resync_items(
    db: &crate::store::Db,
    asks: &crate::ask::AskRegistry,
) -> Vec<(i32, String)> {
    let mut out = Vec::new();
    for a in asks.open() {
        let title = crate::store::repo::get_thread(db, a.thread)
            .await
            .ok()
            .flatten()
            .map(|t| t.title)
            .unwrap_or_default();
        let label = if title.is_empty() {
            a.summary.clone()
        } else {
            format!("{}：{}", title, a.summary)
        };
        out.push((a.thread, label));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allow_trims_and_drops_empties() {
        assert_eq!(
            ImSettings::parse_allow(" ou_a , ,ou_b,"),
            vec!["ou_a".to_string(), "ou_b".to_string()]
        );
        assert!(ImSettings::parse_allow("").is_empty());
    }

    #[test]
    fn ready_requires_creds() {
        let mut s = ImSettings {
            app_id: "a".into(),
            app_secret: "s".into(),
            ..Default::default()
        };
        assert!(s.ready());
        s.app_secret.clear();
        assert!(!s.ready());
        s = ImSettings::default();
        assert!(!s.ready());
    }

    #[tokio::test]
    async fn settings_load_roundtrip() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        // 未设置时全默认
        let s = ImSettings::load(&db).await.unwrap();
        assert_eq!(s, ImSettings::default());
        assert!(!s.ready());
        // 写入后读回
        crate::store::repo::set_setting(&db, K_APP_ID, "cli_x")
            .await
            .unwrap();
        crate::store::repo::set_setting(&db, K_APP_SECRET, "sec")
            .await
            .unwrap();
        crate::store::repo::set_setting(&db, K_ALLOW, "ou_a, ou_b")
            .await
            .unwrap();
        let s = ImSettings::load(&db).await.unwrap();
        assert!(s.ready());
        assert_eq!(
            s.allow_open_ids,
            vec!["ou_a".to_string(), "ou_b".to_string()]
        );
    }

    #[tokio::test]
    async fn settings_load_propagates_db_errors() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("DROP TABLE app_setting")
            .await
            .unwrap();
        // DB 错误必须传播为 Err（fail-closed），不得折叠成默认设置
        assert!(ImSettings::load(&db).await.is_err());
    }

    #[test]
    fn card_index_roundtrip() {
        let mut c = CardIndex::default();
        c.record_perm(7, "om_1", "Run: npm test");
        c.record_human(3, 9, "om_2");
        assert_eq!(c.target_of("om_1"), Some(ReplyTarget::Perm { ask_id: 7 }));
        assert_eq!(
            c.target_of("om_2"),
            Some(ReplyTarget::Human {
                thread: 3,
                ask_id: 9
            })
        );
        // take_perm 连 summary 一起取回（Resolved 事件不带 summary，终态卡靠这里）
        assert_eq!(
            c.take_perm(7),
            Some(("om_1".to_string(), "Run: npm test".to_string()))
        );
        assert_eq!(c.target_of("om_1"), None); // 反向索引同步清
        assert_eq!(c.take_human(3, 9).as_deref(), Some("om_2"));
        assert_eq!(c.take_perm(7), None);
    }

    #[test]
    fn rerecord_clears_old_reverse_index() {
        let mut c = CardIndex::default();
        c.record_perm(7, "om_1", "s1");
        c.record_perm(7, "om_1b", "s2");
        assert_eq!(c.target_of("om_1"), None); // 旧 message_id 不再可路由
        assert_eq!(c.target_of("om_1b"), Some(ReplyTarget::Perm { ask_id: 7 }));
        c.record_human(3, 9, "om_2");
        c.record_human(3, 9, "om_2b");
        assert_eq!(c.target_of("om_2"), None);
        assert_eq!(
            c.target_of("om_2b"),
            Some(ReplyTarget::Human {
                thread: 3,
                ask_id: 9
            })
        );
        assert_eq!(
            c.take_perm(7),
            Some(("om_1b".to_string(), "s2".to_string()))
        );
        assert_eq!(c.take_human(3, 9).as_deref(), Some("om_2b"));
    }

    #[tokio::test]
    async fn build_resync_items_pairs_thread_titles_with_summaries() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let asks = crate::ask::AskRegistry::new();
        let w = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let t1 = crate::store::repo::create_thread(&db, w.id, "登录修复", "bugfix", "claude")
            .await
            .unwrap();
        let t2 = crate::store::repo::create_thread(&db, w.id, "结算优化", "feature", "claude")
            .await
            .unwrap();
        let _ = asks.request(t1.id, "10", "claude", "Run: npm test", "npm test");
        let _ = asks.request(t2.id, "20", "codex", "Edit src/foo.rs", "src/foo.rs");

        let items = build_resync_items(&db, &asks).await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, t1.id);
        assert!(items[0].1.starts_with("登录修复："));
        assert!(items[0].1.ends_with("Run: npm test"));
        assert_eq!(items[1].0, t2.id);
        assert!(items[1].1.contains("结算优化"));
    }

    #[tokio::test]
    async fn build_resync_items_empty_when_no_open_asks() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let asks = crate::ask::AskRegistry::new();
        assert!(build_resync_items(&db, &asks).await.is_empty());
    }

    #[tokio::test]
    async fn build_resync_items_falls_back_when_thread_row_missing() {
        // 异常路径：DB 里没有这个 thread 行（route 残留），label 退化为 summary。
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let asks = crate::ask::AskRegistry::new();
        let _ = asks.request(999, "10", "claude", "Run: npm test", "npm test");
        let items = build_resync_items(&db, &asks).await;
        assert_eq!(items, vec![(999, "Run: npm test".to_string())]);
    }

    #[tokio::test]
    async fn im_concierge_thread_uses_effective_default_tool() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        crate::store::repo::set_setting(&db, "default_tool", "codex")
            .await
            .unwrap();
        let expected = crate::tools::default_tool(&db).await;

        let thread_id = ensure_im_concierge_thread(&db, "ou_owner", "oc_dm", "dm:ou_owner")
            .await
            .unwrap();

        let thread = crate::store::repo::get_thread(&db, thread_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(thread.kind, "concierge");
        assert_eq!(thread.lead_tool, expected);
    }
}
