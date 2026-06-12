//! 飞书 CardKit 流式卡片渲染（raw REST）。
//!
//! 为什么 raw 而非 typed SDK：openlark 0.17 的 `UpdateCardElementContentBody` 只有
//! `{card_id, element_id, content}`，**缺 Feishu 流式协议必需的 `sequence`**——实测
//! 不带 sequence 直接 `code 99992402 field validation failed`。故流式追加必须自造请求。
//! 既然 content 必走 raw，create/send 也一并放这里保持内聚，独立缓存 tenant_access_token。
//!
//! 流程：`create_streaming_card`（建带 streaming_mode 的卡实体）→ `send_entity_card`
//! （把卡 entity 发给收件人，拿 message_id）→ 多次 `put_content`（带递增 sequence 追加
//! 文本，客户端渲染打字机）→ `finalize`（关 streaming_mode，停掉「正在输入」指示）。
//!
//! `Channel::stream_*` 默认 no-op；飞书通道会走这里。

use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const BASE: &str = "https://open.feishu.cn";
/// 流式卡里那个 markdown 文本组件的固定 id（create 与 put_content 共享）。
pub const ELEMENT_ID: &str = "md_streaming";
/// 卡片体上限 30KB；CJK 取 9000 字符 ≈ 27KB。超限前先截断。
const MAX_STREAM_CHARS: usize = 9000;

/// 按字符截断（CJK 安全：字节切片落在多字节字符中间会 panic，生产路径 deny panic）。
fn clamp(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("…(truncated)");
        out
    }
}

/// 一张流式卡的运行态：id 三元组 + 严格递增的 sequence + 去重缓存。
/// 桥侧按 thread_id 持有一份；`next_seq`/`should_send`/`mark_sent` 是纯逻辑（单测覆盖）。
#[derive(Debug, Clone)]
pub struct StreamSession {
    pub card_id: String,
    pub element_id: String,
    pub message_id: String,
    seq: u64,
    last_sent: Option<String>,
}

impl StreamSession {
    pub fn new(card_id: String, element_id: String, message_id: String) -> Self {
        Self {
            card_id,
            element_id,
            message_id,
            seq: 0,
            last_sent: None,
        }
    }
    /// Feishu 要求 sequence 严格递增；每次发送前取下一个。
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    /// 内容相对上次发送有变化才值得再发一次卡片更新（省配额）。
    pub fn should_send(&self, text: &str) -> bool {
        self.last_sent.as_deref() != Some(text)
    }
    pub fn mark_sent(&mut self, text: &str) {
        self.last_sent = Some(text.to_string());
    }
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// 流式卡的 HTTPS 客户端：自管 tenant_access_token 缓存（区别于 typed SDK 的 token 缓存，
/// 因为 raw PUT 不经过 SDK transport）。
pub struct StreamClient {
    app_id: String,
    app_secret: String,
    http: reqwest::Client,
    token: Mutex<Option<CachedToken>>,
}

impl StreamClient {
    pub fn new(app_id: &str, app_secret: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            http: reqwest::Client::new(),
            token: Mutex::new(None),
        }
    }

    /// 拿一个有效 token：缓存命中（留 60s 余量）直接用，否则取新的并缓存。
    async fn token(&self) -> anyhow::Result<String> {
        {
            let g = self.token.lock().await;
            if let Some(t) = g.as_ref() {
                if t.expires_at > Instant::now() {
                    return Ok(t.token.clone());
                }
            }
        }
        let resp: serde_json::Value = self
            .http
            .post(format!(
                "{BASE}/open-apis/auth/v3/tenant_access_token/internal"
            ))
            .json(&serde_json::json!({"app_id": self.app_id, "app_secret": self.app_secret}))
            .send()
            .await?
            .json()
            .await?;
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "feishu token: code={code} msg={}",
                resp.get("msg").and_then(|m| m.as_str()).unwrap_or("?")
            );
        }
        let token = resp
            .get("tenant_access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("feishu token: no tenant_access_token"))?
            .to_string();
        let expire = resp.get("expire").and_then(|e| e.as_u64()).unwrap_or(7200);
        *self.token.lock().await = Some(CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expire.saturating_sub(60)),
        });
        Ok(token)
    }

    /// 飞书统一返回体的 code 校验（0 = ok），失败带 msg 抛错。
    fn check(resp: &serde_json::Value, what: &str) -> anyhow::Result<()> {
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "feishu {what}: code={code} msg={}",
                resp.get("msg").and_then(|m| m.as_str()).unwrap_or("?")
            );
        }
        Ok(())
    }

    /// 建带 streaming_mode 的卡实体，返回 card_id。
    pub async fn create_streaming_card(&self) -> anyhow::Result<String> {
        let card_json = serde_json::json!({
            "schema": "2.0",
            "config": {"streaming_mode": true, "summary": {"content": ""}},
            "body": {"elements": [{"tag": "markdown", "content": "", "element_id": ELEMENT_ID}]}
        });
        let token = self.token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{BASE}/open-apis/cardkit/v1/cards"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"type": "card_json", "data": card_json.to_string()}))
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "create_card")?;
        resp.get("data")
            .and_then(|d| d.get("card_id"))
            .and_then(|c| c.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("feishu create_card: no card_id"))
    }

    /// 把卡 entity 发给收件人，返回 message_id。`receive_id_type` 取 "open_id" / "chat_id"。
    pub async fn send_entity_card(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        card_id: &str,
    ) -> anyhow::Result<String> {
        let content = serde_json::json!({"type": "card", "data": {"card_id": card_id}}).to_string();
        let token = self.token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{BASE}/open-apis/im/v1/messages"))
            .query(&[("receive_id_type", receive_id_type)])
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "receive_id": receive_id, "msg_type": "interactive", "content": content
            }))
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "send_entity_card")?;
        resp.get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("feishu send_entity_card: no message_id"))
    }

    /// 把卡 entity 作为**回复**挂到 `reply_to`（话题根 message_id）下，返回新消息 id。
    /// 用于 issue 话题：lead 的流式卡挂在话题里，与一次性 reply_text 同一线程语义。
    pub async fn reply_entity_card(&self, reply_to: &str, card_id: &str) -> anyhow::Result<String> {
        let content = serde_json::json!({"type": "card", "data": {"card_id": card_id}}).to_string();
        let token = self.token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{BASE}/open-apis/im/v1/messages/{reply_to}/reply"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"msg_type": "interactive", "content": content}))
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "reply_entity_card")?;
        resp.get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("feishu reply_entity_card: no message_id"))
    }

    /// 流式追加文本：PUT content + 必需的递增 sequence。content 为 markdown 全文。
    pub async fn put_content(
        &self,
        card_id: &str,
        element_id: &str,
        content: &str,
        sequence: u64,
    ) -> anyhow::Result<()> {
        let body_content = clamp(content, MAX_STREAM_CHARS);
        let token = self.token().await?;
        let resp: serde_json::Value = self
            .http
            .put(format!(
                "{BASE}/open-apis/cardkit/v1/cards/{card_id}/elements/{element_id}/content"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({"content": body_content, "sequence": sequence}))
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "put_content")
    }

    /// 收尾：关掉 streaming_mode，停掉客户端「正在输入」指示。失败不致命（卡片已成稿）。
    /// 实测 settings 体形态：`settings` 须为**字符串化** JSON，且和 content 一样需要
    /// 递增 `sequence`（不带或传对象都 99992402）。
    pub async fn finalize(&self, card_id: &str, sequence: u64) -> anyhow::Result<()> {
        let token = self.token().await?;
        let settings = serde_json::json!({"config": {"streaming_mode": false}}).to_string();
        let resp: serde_json::Value = self
            .http
            .patch(format!(
                "{BASE}/open-apis/cardkit/v1/cards/{card_id}/settings"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({"settings": settings, "sequence": sequence}))
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "finalize")
    }

    /// 删除消息表情回复。typed openlark 0.17 会把成功但无 `data` 的响应当成
    /// 验证错误；这里用 raw REST 只校验飞书通用 `code`。
    pub async fn delete_message_reaction(
        &self,
        message_id: &str,
        reaction_id: &str,
    ) -> anyhow::Result<()> {
        let token = self.token().await?;
        let resp: serde_json::Value = self
            .http
            .delete(format!(
                "{BASE}/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}"
            ))
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;
        Self::check(&resp, "delete_message_reaction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_session_sequence_is_monotonic() {
        let mut s = StreamSession::new("c".into(), ELEMENT_ID.into(), "m".into());
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.next_seq(), 2);
        assert_eq!(s.next_seq(), 3);
    }

    #[test]
    fn stream_session_dedups_unchanged_text() {
        let mut s = StreamSession::new("c".into(), ELEMENT_ID.into(), "m".into());
        assert!(s.should_send("hello")); // 首次：发
        s.mark_sent("hello");
        assert!(!s.should_send("hello")); // 没变：跳过
        assert!(s.should_send("hello!")); // 变了：发
    }

    #[test]
    fn check_accepts_success_without_data() {
        let resp = serde_json::json!({"code": 0, "msg": "success"});
        assert!(StreamClient::check(&resp, "delete_message_reaction").is_ok());
    }

    #[test]
    fn check_rejects_nonzero_code() {
        let resp = serde_json::json!({"code": 230001, "msg": "reaction not found"});
        let err = StreamClient::check(&resp, "delete_message_reaction").unwrap_err();
        assert!(err.to_string().contains("230001"));
    }
}
