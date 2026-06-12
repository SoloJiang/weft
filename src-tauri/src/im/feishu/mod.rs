//! 飞书 Channel 适配器：REST 发卡/patch/发文本（openlark 0.17 `communication::im` v1）。
//! 长连接入站在 ws.rs。API 以 openlark 0.17 实测：
//! - `CreateMessageRequest::new(cfg).receive_id_type(..).execute_with_options(body, opt) -> SDKResult<Value>`
//! - `ReplyMessageRequest::new(cfg).message_id(id).execute_with_options(ReplyMessageBody, opt) -> SDKResult<Value>`
//! - `PatchMessageCardRequest::new(cfg).message_id(id).execute(json!({"content": <卡片串>}))`
//! - `CreateMessageReactionRequest`/`DeleteMessageReactionRequest`（0.17 起 `MessageReaction` 回
//!   `reaction_id`，故 👀 可被真正删除——0.14 只能回空串跳过删除）。
//!
//! 旧 `open-lark` 0.14 的 `LarkClient`/`service::im` 在 0.17 里改名为 `Client`/`communication::im`，
//! 调用风格也从 `client.im.v1.message.create(req, None)` 改为「请求构建器 + `.execute*(body)`」。
//! `app_type` 默认即 `SelfBuild`（openlark_core `AppType` 的 `#[default]`），故无需显式设置。

pub mod streaming;
pub mod ws;

use open_lark::communication::im::v1::message::create::{CreateMessageBody, CreateMessageRequest};
use open_lark::communication::im::v1::message::models::ReceiveIdType;
use open_lark::communication::im::v1::message::patch::PatchMessageCardRequest;
use open_lark::communication::im::v1::message::reaction::create::CreateMessageReactionRequest;
use open_lark::communication::im::v1::message::reaction::models::{
    CreateMessageReactionBody, ReactionType,
};
use open_lark::communication::im::v1::message::reply::{ReplyMessageBody, ReplyMessageRequest};
use open_lark::{ClientBuilder, CoreConfig, RequestOption};

pub struct FeishuChannel {
    // 0.17 的 REST 请求构建器以 `CoreConfig`（openlark_core::config::Config）构造
    // ——区别于长连接用的 `open_lark::Config`（openlark_client）。缓存一份可克隆的
    // CoreConfig 而非整个 Client：token 缓存挂在内部，clone 共享同一缓存。
    config: CoreConfig,
    // 流式卡片走 raw REST（typed SDK 的 content body 缺 sequence），独立 token 缓存。
    stream: streaming::StreamClient,
}

impl FeishuChannel {
    /// 构造适配器。0.17 的 `ClientBuilder::build()` 返回 `Result`（凭证校验），故本函数
    /// 也返回 `Result`，调用方需传播。
    pub fn new(app_id: &str, app_secret: &str) -> anyhow::Result<Self> {
        let client = ClientBuilder::new()
            .app_id(app_id)
            .app_secret(app_secret)
            .enable_token_cache(true)
            .build()
            .map_err(|e| anyhow::anyhow!("feishu client build: {e}"))?;
        Ok(Self {
            config: client.config().clone(),
            stream: streaming::StreamClient::new(app_id, app_secret),
        })
    }

    /// 从 0.17 send/reply 返回的 Value 里取 message_id（兼容「已抽 data」与「原始响应」两种形态）。
    fn message_id_of(resp: &serde_json::Value) -> anyhow::Result<String> {
        resp.get("message_id")
            .or_else(|| resp.get("data").and_then(|d| d.get("message_id")))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("feishu: no message_id in response {resp}"))
    }

    /// 发消息（msg_type 由调用方定，content 为序列化好的 JSON 字符串）。返回飞书 message_id。
    async fn create(
        &self,
        receive_id_type: ReceiveIdType,
        receive_id: &str,
        msg_type: &str,
        content: String,
    ) -> anyhow::Result<String> {
        let body = CreateMessageBody {
            receive_id: receive_id.to_string(),
            msg_type: msg_type.to_string(),
            content,
            uuid: None,
        };
        let resp = CreateMessageRequest::new(self.config.clone())
            .receive_id_type(receive_id_type)
            .execute_with_options(body, RequestOption::default())
            .await
            .map_err(|e| anyhow::anyhow!("feishu create({msg_type}): {e}"))?;
        Self::message_id_of(&resp)
    }
}

#[async_trait::async_trait]
impl super::Channel for FeishuChannel {
    async fn send_card(&self, open_id: &str, card: serde_json::Value) -> anyhow::Result<String> {
        self.create(
            ReceiveIdType::OpenId,
            open_id,
            "interactive",
            card.to_string(),
        )
        .await
    }

    async fn patch_card(&self, message_id: &str, card: serde_json::Value) -> anyhow::Result<()> {
        // 0.17 没有独立的 message_card 模块：更新卡片即 message patch，body 形态
        // `{"content": "<卡片 JSON 串>"}`（详见 patch.rs 文档：content 须为序列化后的字符串）。
        PatchMessageCardRequest::new(self.config.clone())
            .message_id(message_id)
            .execute(serde_json::json!({ "content": card.to_string() }))
            .await
            .map_err(|e| anyhow::anyhow!("feishu patch_card: {e}"))?;
        Ok(())
    }

    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()> {
        let content = serde_json::json!({ "text": text }).to_string();
        self.create(ReceiveIdType::OpenId, open_id, "text", content)
            .await?;
        Ok(())
    }

    async fn send_chat_text(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        self.create(ReceiveIdType::ChatId, chat_id, "text", content)
            .await
    }

    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String> {
        // 飞书 reply API：传入话题内任意消息 id，回复自动挂同一话题下
        // （0.17：`POST /im/v1/messages/:message_id/reply`）。
        let content = serde_json::json!({ "text": text }).to_string();
        let body = ReplyMessageBody {
            content,
            msg_type: "text".to_string(),
            reply_in_thread: None,
            uuid: None,
        };
        let resp = ReplyMessageRequest::new(self.config.clone())
            .message_id(reply_to)
            .execute_with_options(body, RequestOption::default())
            .await
            .map_err(|e| anyhow::anyhow!("feishu reply: {e}"))?;
        Self::message_id_of(&resp)
    }

    async fn add_reaction(&self, message_id: &str, emoji: &str) -> anyhow::Result<String> {
        // 0.17 的 `MessageReaction` 带 `reaction_id`——回真实 id，下游可真正删除（取下 👀）。
        let reaction = CreateMessageReactionRequest::new(self.config.clone())
            .message_id(message_id)
            .execute(CreateMessageReactionBody {
                reaction_type: ReactionType {
                    emoji_type: emoji.to_string(),
                },
            })
            .await
            .map_err(|e| anyhow::anyhow!("feishu add_reaction: {e}"))?;
        Ok(reaction.reaction_id)
    }

    async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
        if reaction_id.is_empty() {
            return Ok(()); // add_reaction 没回 id：跳过 delete。
        }
        self.stream
            .delete_message_reaction(message_id, reaction_id)
            .await
            .map_err(|e| anyhow::anyhow!("feishu delete_reaction: {e}"))?;
        Ok(())
    }

    async fn stream_begin(
        &self,
        receive_id_type: &str,
        receive_id: &str,
    ) -> anyhow::Result<Option<streaming::StreamSession>> {
        let card_id = self.stream.create_streaming_card().await?;
        let message_id = self
            .stream
            .send_entity_card(receive_id_type, receive_id, &card_id)
            .await?;
        Ok(Some(streaming::StreamSession::new(
            card_id,
            streaming::ELEMENT_ID.to_string(),
            message_id,
        )))
    }

    async fn stream_begin_reply(
        &self,
        reply_to: &str,
    ) -> anyhow::Result<Option<streaming::StreamSession>> {
        let card_id = self.stream.create_streaming_card().await?;
        let message_id = self.stream.reply_entity_card(reply_to, &card_id).await?;
        Ok(Some(streaming::StreamSession::new(
            card_id,
            streaming::ELEMENT_ID.to_string(),
            message_id,
        )))
    }

    async fn stream_push(
        &self,
        session: &mut streaming::StreamSession,
        accumulated: &str,
    ) -> anyhow::Result<()> {
        // 内容没变就不发——省一次卡片更新配额。
        if !session.should_send(accumulated) {
            return Ok(());
        }
        let seq = session.next_seq();
        self.stream
            .put_content(&session.card_id, &session.element_id, accumulated, seq)
            .await?;
        session.mark_sent(accumulated);
        Ok(())
    }

    async fn stream_end(
        &self,
        session: &mut streaming::StreamSession,
        final_text: &str,
    ) -> anyhow::Result<()> {
        // 最后一帧强制发权威全文（绕过去重），再关 streaming_mode。
        let seq = session.next_seq();
        self.stream
            .put_content(&session.card_id, &session.element_id, final_text, seq)
            .await?;
        session.mark_sent(final_text);
        // 关流式模式失败不致命：卡片内容已是终态，只是少了「停掉输入指示」这步。
        // settings 也吃同一序号空间里的下一个 sequence。
        let fin_seq = session.next_seq();
        if let Err(e) = self.stream.finalize(&session.card_id, fin_seq).await {
            eprintln!("[weft][im] stream finalize: {e}");
        }
        Ok(())
    }
}
