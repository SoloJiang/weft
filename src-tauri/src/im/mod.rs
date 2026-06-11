//! IM 桥（spec: docs/superpowers/specs/2026-06-11-im-feishu-integration-design.md）。
//! 通道无关核心：设置、卡片索引、Channel trait、入站执行、桥运行时。
//! feishu/ 是第一个适配器。结构化动作全走确定性代码，LLM 不在路径上。

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
    perm_msg: HashMap<u64, String>,
    human_msg: HashMap<(i32, u64), String>,
    by_message: HashMap<String, ReplyTarget>,
}

impl CardIndex {
    pub fn record_perm(&mut self, ask_id: u64, message_id: &str) {
        if let Some(old) = self.perm_msg.insert(ask_id, message_id.to_string()) {
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
    /// 解决后取走（patch 终态用），并清反向索引。
    pub fn take_perm(&mut self, ask_id: u64) -> Option<String> {
        let m = self.perm_msg.remove(&ask_id)?;
        self.by_message.remove(&m);
        Some(m)
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
        c.record_perm(7, "om_1");
        c.record_human(3, 9, "om_2");
        assert_eq!(c.target_of("om_1"), Some(ReplyTarget::Perm { ask_id: 7 }));
        assert_eq!(c.target_of("om_2"), Some(ReplyTarget::Human { thread: 3, ask_id: 9 }));
        assert_eq!(c.take_perm(7).as_deref(), Some("om_1"));
        assert_eq!(c.target_of("om_1"), None); // 反向索引同步清
        assert_eq!(c.take_human(3, 9).as_deref(), Some("om_2"));
        assert_eq!(c.take_perm(7), None);
    }

    #[test]
    fn rerecord_clears_old_reverse_index() {
        let mut c = CardIndex::default();
        c.record_perm(7, "om_1");
        c.record_perm(7, "om_1b");
        assert_eq!(c.target_of("om_1"), None); // 旧 message_id 不再可路由
        assert_eq!(c.target_of("om_1b"), Some(ReplyTarget::Perm { ask_id: 7 }));
        c.record_human(3, 9, "om_2");
        c.record_human(3, 9, "om_2b");
        assert_eq!(c.target_of("om_2"), None);
        assert_eq!(c.target_of("om_2b"), Some(ReplyTarget::Human { thread: 3, ask_id: 9 }));
        assert_eq!(c.take_perm(7).as_deref(), Some("om_1b"));
        assert_eq!(c.take_human(3, 9).as_deref(), Some("om_2b"));
    }
}
