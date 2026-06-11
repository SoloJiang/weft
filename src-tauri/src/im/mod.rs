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

#[derive(Clone, Default, PartialEq)]
pub struct ImSettings {
    pub app_id: String,
    pub app_secret: String,
    pub allow_open_ids: Vec<String>,
}

impl std::fmt::Debug for ImSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImSettings")
            .field("app_id", &self.app_id)
            .field("app_secret", &if self.app_secret.is_empty() { "" } else { "***" })
            .field("allow_open_ids", &self.allow_open_ids)
            .finish()
    }
}

impl ImSettings {
    /// 桥能否启动：双凭证齐全即跑——无独立 enable 开关（spec MVP 单机即信任边界）。
    pub fn ready(&self) -> bool {
        !self.app_id.is_empty() && !self.app_secret.is_empty()
    }

    pub fn parse_allow(s: &str) -> Vec<String> {
        s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
    }

    /// 从 app_setting 读取设置。「键不存在」是默认值；DB 错误原样传播。
    /// Err 必须 fail-closed：桥侧把 Err 当连接错误处理，绝不当作未配置/空白名单
    /// （否则瞬时 DB 错误会清空白名单，导致首个私聊发送者被自动绑定）。
    pub async fn load(db: &crate::store::Db) -> anyhow::Result<Self> {
        use crate::store::repo::get_setting;
        let g = |k: &'static str| async move {
            anyhow::Ok(get_setting(db, k).await?.unwrap_or_default())
        };
        Ok(Self {
            app_id: g(K_APP_ID).await?,
            app_secret: g(K_APP_SECRET).await?,
            allow_open_ids: Self::parse_allow(&g(K_ALLOW).await?),
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
        if let Some((old, _)) =
            self.perm_msg.insert(ask_id, (message_id.to_string(), summary.to_string()))
        {
            self.by_message.remove(&old);
        }
        self.by_message.insert(message_id.to_string(), ReplyTarget::Perm { ask_id });
    }
    pub fn record_human(&mut self, thread: i32, ask_id: u64, message_id: &str) {
        if let Some(old) = self.human_msg.insert((thread, ask_id), message_id.to_string()) {
            self.by_message.remove(&old);
        }
        self.by_message.insert(message_id.to_string(), ReplyTarget::Human { thread, ask_id });
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

/// IM 通道抽象（spec §2.1）：M1 仅飞书实现 + 测试替身。能力开关后续随
/// 第二通道引入（M1 飞书全支持，YAGNI）。
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// 发交互卡片到用户（p2p），返回 message_id。
    async fn send_card(&self, open_id: &str, card: serde_json::Value) -> anyhow::Result<String>;
    /// 把已发卡片 patch 成终态。
    async fn patch_card(&self, message_id: &str, card: serde_json::Value) -> anyhow::Result<()>;
    /// 发纯文本到用户（p2p）。
    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()>;
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
    async fn delete_reaction(
        &self,
        _message_id: &str,
        _reaction_id: &str,
    ) -> anyhow::Result<()> {
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
        inbound::Route::Bind { open_id } => {
            // Route 读的是 allow 快照；落库前重查仍为空（Route::Bind doc 的竞态契约）。
            let cur = crate::store::repo::get_setting(db, K_ALLOW).await?.unwrap_or_default();
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
        inbound::Route::AnswerHuman { thread, ask_id, text } => {
            if !bus.answer_ask(thread, ask_id, &text) {
                if let Err(e) = channel
                    .send_text(
                        sender,
                        t("这个提问已被回答过了。", "That question was already answered."),
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
        inbound::Route::FreeText { sender_open_id, text } => {
            // M3: 接 Concierge engine。无 app 句柄（测试路径）或 Concierge 未就绪
            // 时退化成提示。Concierge 入口通过 lead_chat thread_id=0（spec §5 M3-1）
            // 在 app 上挂载。
            let _ = (&sender_open_id, &text);
            if let Some(app) = app {
                if let Err(e) = consume_free_text(app, db, &sender_open_id, &text, lang).await {
                    eprintln!("[weft][im] concierge: {e}");
                }
            } else if let Err(e) = channel
                .send_text(
                    sender,
                    t(
                        "自由对话（全局助理）将在后续版本上线；当前请回复卡片消息作答权限与提问。",
                        "Free chat (the global concierge) lands in a later milestone; for now reply to cards.",
                    ),
                )
                .await
            {
                eprintln!("[weft][im] freetext hint: {e}");
            }
        }
        inbound::Route::IssueMessage { chat_id, im_thread_ref, sender_open_id: _, text } => {
            // 飞书话题里的消息 → 反查 im_route 命中 issue → 灌进 lead engine。
            // 未绑定（话题尚未 bind 过 issue）静默忽略：M2-5 提供桌面/IM 主动绑定入口。
            let r =
                crate::store::repo::im_route_of_thread_ref(db, "feishu", &chat_id, &im_thread_ref)
                    .await?;
            let Some(route) = r else { return Ok(()) };
            // M2-6 回执：在投递 engine 之前先挂 👀——出站前批量 delete。
            // ctx 没给 message_id / acks 则跳过；reaction add 失败不阻挡后续灌入。
            if let (Some(ctx), true) =
                (ctx, ctx.map(|c| c.inbound_message_id.is_some()).unwrap_or(false))
            {
                if let (Some(mid), Some(acks)) =
                    (ctx.inbound_message_id.as_deref(), ctx.acks.as_ref())
                {
                    match channel.add_reaction(mid, "EYES").await {
                        Ok(rid) => {
                            acks.lock().await.entry(route.thread_id).or_default()
                                .push((mid.to_string(), rid));
                        }
                        Err(e) => eprintln!("[weft][im] add reaction: {e}"),
                    }
                }
            }
            let Some(app) = app else { return Ok(()) }; // 测试路径不进 engine
            if let Err(e) =
                feed_issue_message(app, db, route.thread_id, &text, lang).await
            {
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
        if g.status.is_empty() { "disabled".to_string() } else { g.status.clone() }
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
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).generation == generation
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
        if !settings.ready() {
            bridge.set_status("disabled");
            return;
        }
        bridge.set_status("connecting");

        let channel: Arc<dyn Channel> =
            Arc::new(feishu::FeishuChannel::new(&settings.app_id, &settings.app_secret));

        // —— 出站：registry 通知 → 发卡/patch ——
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel();
        let (hum_tx, mut hum_rx) = tokio::sync::mpsc::unbounded_channel();
        // set_notifier 返回挂接瞬间已 open 的快照：桥重启时补发卡片（无 miss/dup）。
        let snapshot = app.state::<crate::ask::AskRegistry>().set_notifier(ask_tx);
        app.state::<crate::bus::BusRegistry>().set_ask_notifier(hum_tx);
        {
            let (app2, db2, ch, cards2) = (app.clone(), db.clone(), channel.clone(), cards.clone());
            tauri::async_runtime::spawn(async move {
                let bridge = app2.state::<ImBridge>();
                // 先补发快照里的已开 Ask（挂接前就 open 的，不会再有 Opened 事件）。
                for ask in snapshot {
                    if !bridge.live(generation) {
                        return;
                    }
                    consume_ask_event(crate::ask::AskEvent::Opened(ask), &db2, ch.as_ref(), &cards2)
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
            let (app2, db2, ch, cards2, acks2) =
                (app.clone(), db.clone(), channel.clone(), cards.clone(), acks.clone());
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
                        inbound::Inbound::Text { sender_open_id, message_id, .. } => {
                            (sender_open_id.clone(), Some(message_id.clone()))
                        }
                        inbound::Inbound::Action { operator_open_id, .. } => {
                            (operator_open_id.clone(), None)
                        }
                    };
                    let r = { inbound::route(&inb, &allow, &*cards2.lock().await) };
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
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
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
                loop {
                    if !bridge.live(generation) {
                        return;
                    }
                    bridge.set_status("online"); // 连接建立细节在 run_ws 内
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
            match ch.send_card(&owner, outbound::human_card(&title, &from, &ask.text, IM_LANG)).await
            {
                Ok(mid) => cards.lock().await.record_human(thread, ask.id, &mid),
                Err(e) => eprintln!("[weft][im] send human card: {e}"),
            }
        }
        crate::bus::state::HumanAskEvent::Answered { thread, ask_id, text } => {
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

/// M2-4: lead engine 的 assistant 文本完成 → 反查 im_route → 飞书话题 reply。
/// 同时把这个 thread 挂账的 👀 reactions 一次性 delete（spec §4 回执语义：
/// 「轮到这条被回复」才取下 👀，排队期间一直在）。pub 给集成测试用。
pub async fn consume_lead_out(
    out: crate::lead_chat::out_hub::LeadOut,
    db: &crate::store::Db,
    ch: &dyn Channel,
    acks: &Arc<tokio::sync::Mutex<HashMap<i32, Vec<(String, String)>>>>,
) {
    // 反查 im_route：thread 没绑定 → engine 文本只走桌面，不上桥。
    let route = match crate::store::repo::im_route_of_thread(db, out.thread_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            eprintln!("[weft][im] lead-out lookup route: {e}");
            return;
        }
    };
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

/// M3-3: 单聊自由文本 → Concierge engine（lead_chat thread_id=0 占位）。
/// 占位实现：未配置 Concierge 时退化为提示——M3-1 把 thread_id=0 的 lead 装上。
async fn consume_free_text(
    app: &tauri::AppHandle,
    db: &crate::store::Db,
    sender_open_id: &str,
    text: &str,
    lang: &str,
) -> anyhow::Result<()> {
    // 确保 Concierge thread 存在并拿到 id（spec §5 占位 thread；首次自动建 workspace）。
    let thread_id = crate::store::repo::ensure_concierge_thread(db).await?;
    let eng = crate::lead_chat::commands::lead_engine(app, db, thread_id, lang).await?;
    let framed = format!("[from {sender_open_id}] {text}");
    crate::lead_chat::engine::send(app, db, &eng, &framed, Vec::new(), Vec::new()).await
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
        let mut s = ImSettings { app_id: "a".into(), app_secret: "s".into(), ..Default::default() };
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
        crate::store::repo::set_setting(&db, K_APP_ID, "cli_x").await.unwrap();
        crate::store::repo::set_setting(&db, K_APP_SECRET, "sec").await.unwrap();
        crate::store::repo::set_setting(&db, K_ALLOW, "ou_a, ou_b").await.unwrap();
        let s = ImSettings::load(&db).await.unwrap();
        assert!(s.ready());
        assert_eq!(s.allow_open_ids, vec!["ou_a".to_string(), "ou_b".to_string()]);
    }

    #[tokio::test]
    async fn settings_load_propagates_db_errors() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("DROP TABLE app_setting").await.unwrap();
        // DB 错误必须传播为 Err（fail-closed），不得折叠成默认设置
        assert!(ImSettings::load(&db).await.is_err());
    }

    #[test]
    fn card_index_roundtrip() {
        let mut c = CardIndex::default();
        c.record_perm(7, "om_1", "Run: npm test");
        c.record_human(3, 9, "om_2");
        assert_eq!(c.target_of("om_1"), Some(ReplyTarget::Perm { ask_id: 7 }));
        assert_eq!(c.target_of("om_2"), Some(ReplyTarget::Human { thread: 3, ask_id: 9 }));
        // take_perm 连 summary 一起取回（Resolved 事件不带 summary，终态卡靠这里）
        assert_eq!(c.take_perm(7), Some(("om_1".to_string(), "Run: npm test".to_string())));
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
        assert_eq!(c.target_of("om_2b"), Some(ReplyTarget::Human { thread: 3, ask_id: 9 }));
        assert_eq!(c.take_perm(7), Some(("om_1b".to_string(), "s2".to_string())));
        assert_eq!(c.take_human(3, 9).as_deref(), Some("om_2b"));
    }
}
