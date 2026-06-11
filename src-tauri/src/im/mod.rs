//! IM 桥（spec: docs/superpowers/specs/2026-06-11-im-feishu-integration-design.md）。
//! 通道无关核心：设置、卡片索引、Channel trait、入站执行、桥运行时。
//! feishu/ 是第一个适配器。结构化动作全走确定性代码，LLM 不在路径上。

pub mod feishu;
pub mod inbound;
pub mod outbound;

use std::collections::HashMap;

pub const K_APP_ID: &str = "im.feishu.app_id";
pub const K_APP_SECRET: &str = "im.feishu.app_secret";
pub const K_ENABLED: &str = "im.feishu.enabled";
/// 白名单：逗号分隔的飞书 open_id；空 = 未绑定（首个私聊发送者自动绑定）。
pub const K_ALLOW: &str = "im.feishu.allow_open_ids";

#[derive(Clone, Default, PartialEq)]
pub struct ImSettings {
    pub app_id: String,
    pub app_secret: String,
    pub enabled: bool,
    pub allow_open_ids: Vec<String>,
}

impl std::fmt::Debug for ImSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImSettings")
            .field("app_id", &self.app_id)
            .field("app_secret", &if self.app_secret.is_empty() { "" } else { "***" })
            .field("enabled", &self.enabled)
            .field("allow_open_ids", &self.allow_open_ids)
            .finish()
    }
}

impl ImSettings {
    pub fn ready(&self) -> bool {
        self.enabled && !self.app_id.is_empty() && !self.app_secret.is_empty()
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
            enabled: g(K_ENABLED).await? == "1",
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
}

/// 路由结果执行：直调既有 registry/repo 函数（卡片应答与桌面同函数，spec §2）。
/// `sender` 是入站消息发送者 open_id，用于提示/确认回执；`lang` 控制提示语言。
/// 出站发送失败仅记日志——应答本身已生效，回执是尽力而为。
pub async fn execute(
    route: inbound::Route,
    db: &crate::store::Db,
    asks: &crate::ask::AskRegistry,
    bus: &crate::bus::BusRegistry,
    channel: &dyn Channel,
    sender: &str,
    lang: &str,
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
        inbound::Route::FreeText => {
            if let Err(e) = channel
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
    fn bump(&self) -> (u64, Arc<tokio::sync::Mutex<CardIndex>>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.generation += 1;
        g.cards = Arc::new(tokio::sync::Mutex::new(CardIndex::default()));
        (g.generation, g.cards.clone())
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
        let (generation, cards) = bridge.bump();
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
            let (app2, db2, ch, cards2) = (app.clone(), db.clone(), channel.clone(), cards.clone());
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
                    let sender = match &inb {
                        inbound::Inbound::Text { sender_open_id, .. } => sender_open_id.clone(),
                        inbound::Inbound::Action { operator_open_id, .. } => operator_open_id.clone(),
                    };
                    let r = { inbound::route(&inb, &allow, &*cards2.lock().await) };
                    let asks = app2.state::<crate::ask::AskRegistry>();
                    let bus = app2.state::<crate::bus::BusRegistry>();
                    if let Err(e) =
                        execute(r, &db2, &asks, &bus, ch.as_ref(), &sender, IM_LANG).await
                    {
                        eprintln!("[weft][im] execute: {e}");
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
    fn ready_requires_enabled_and_creds() {
        let mut s = ImSettings { app_id: "a".into(), app_secret: "s".into(), enabled: true, ..Default::default() };
        assert!(s.ready());
        s.enabled = false;
        assert!(!s.ready());
        s = ImSettings { enabled: true, ..Default::default() };
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
        crate::store::repo::set_setting(&db, K_ENABLED, "1").await.unwrap();
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
