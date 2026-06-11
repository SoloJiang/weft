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

    /// 发 p2p 消息（msg_type 由调用方定，content 为序列化好的 JSON 字符串）。
    /// 返回飞书 message_id。
    async fn create(&self, open_id: &str, msg_type: &str, content: String) -> anyhow::Result<String> {
        let req = CreateMessageRequest::builder()
            .receive_id_type("open_id")
            .request_body(
                CreateMessageRequestBody::builder()
                    .receive_id(open_id)
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
        self.create(open_id, "interactive", card.to_string()).await
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
        self.create(open_id, "text", content).await?;
        Ok(())
    }
}
