//! 飞书 Channel 适配器：REST 发卡/patch/发文本（open-lark im v1）。
//! 长连接入站在 ws.rs。API 以 open-lark 0.14 实测：
//! - `client.im.v1.message.create(req, None) -> SDKResult<Message>`（Message.message_id: String）
//! - `client.im.v1.message_card.patch(id, req, None) -> SDKResult<BaseResponse<EmptyResponse>>`

pub mod ws;

use open_lark::prelude::*;
use open_lark::service::im::v1::message::{CreateMessageRequest, CreateMessageRequestBody};
use open_lark::service::im::v1::message_card::PatchMessageCardRequest;

pub struct FeishuChannel {
    client: LarkClient,
}

impl FeishuChannel {
    pub fn new(app_id: &str, app_secret: &str) -> Self {
        let client = LarkClient::builder(app_id, app_secret)
            .with_app_type(AppType::SelfBuild)
            .with_enable_token_cache(true)
            .build();
        Self { client }
    }

    /// 发消息（msg_type 由调用方定，content 为序列化好的 JSON 字符串）。
    /// 返回飞书 message_id。
    async fn create(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: String,
    ) -> anyhow::Result<String> {
        let req = CreateMessageRequest::builder()
            .receive_id_type(receive_id_type)
            .request_body(
                CreateMessageRequestBody::builder()
                    .receive_id(receive_id)
                    .msg_type(msg_type)
                    .content(content)
                    .build(),
            )
            .build();
        let msg = self
            .client
            .im
            .v1
            .message
            .create(req, None)
            .await
            .map_err(|e| anyhow::anyhow!("feishu create({msg_type}): {e}"))?;
        Ok(msg.message_id)
    }
}

#[async_trait::async_trait]
impl super::Channel for FeishuChannel {
    async fn send_card(&self, open_id: &str, card: serde_json::Value) -> anyhow::Result<String> {
        self.create("open_id", open_id, "interactive", card.to_string())
            .await
    }

    async fn patch_card(&self, message_id: &str, card: serde_json::Value) -> anyhow::Result<()> {
        let req = PatchMessageCardRequest { card, token: None };
        self.client
            .im
            .v1
            .message_card
            .patch(message_id, req, None)
            .await
            .map_err(|e| anyhow::anyhow!("feishu patch_card: {e}"))?;
        Ok(())
    }

    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()> {
        let content = serde_json::json!({ "text": text }).to_string();
        self.create("open_id", open_id, "text", content).await?;
        Ok(())
    }

    async fn send_chat_text(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        self.create("chat_id", chat_id, "text", content).await
    }

    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String> {
        // 飞书 reply API：传入任意话题内消息 id，回复自动挂同一话题下
        // （open-lark 0.14：im.v1.message.reply）。
        let content = serde_json::json!({ "text": text }).to_string();
        let req = CreateMessageRequest::builder()
            .request_body(
                CreateMessageRequestBody::builder()
                    .msg_type("text")
                    .content(content)
                    .build(),
            )
            .build();
        let msg = self
            .client
            .im
            .v1
            .message
            .reply(reply_to, req, None)
            .await
            .map_err(|e| anyhow::anyhow!("feishu reply: {e}"))?;
        Ok(msg.message_id)
    }

    async fn add_reaction(&self, message_id: &str, emoji: &str) -> anyhow::Result<String> {
        // open-lark 0.14 的 message_reaction.create 把响应解到 EmptyResponse，
        // 不直接回 reaction_id——按 id 删的能力要等适配器走原始 REST。当前回
        // 占位空串，调用方据此跳过后续 delete。lead 首条 reply 上来后这条 👀
        // 会被话题流挤下去，语义虽不再 100% 严格但已不会误导。
        let _ = self
            .client
            .im
            .v1
            .message_reaction
            .create(message_id, emoji, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("feishu add_reaction: {e}"))?;
        Ok(String::new())
    }

    async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> anyhow::Result<()> {
        if reaction_id.is_empty() {
            return Ok(()); // add_reaction 没回 id：跳过 delete。
        }
        let _ = self
            .client
            .im
            .v1
            .message_reaction
            .delete(message_id, reaction_id, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("feishu delete_reaction: {e}"))?;
        Ok(())
    }
}
