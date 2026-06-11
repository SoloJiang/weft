//! 飞书长连接：事件 → 归一化 Inbound。启动调用以 open-lark 0.14 实测 API：
//! `LarkWsClient::open(Arc<Config>, EventDispatcherHandler)`，阻塞到连接结束；
//! 断线由外层循环指数退避重连。M1 默认无卡片按钮（CARD_BUTTONS=false），
//! 故只订阅 im.message.receive_v1；按钮回调（Action）随 spike 结论 A 再加。

use crate::im::inbound::Inbound;
use open_lark::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

/// "text" 消息的 content 是 {"text":"..."}；其余类型 M1 不收。纯函数。
pub fn text_of(message_type: &str, content: &str) -> Option<String> {
    if message_type != "text" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(content)
        .ok()?
        .get("text")?
        .as_str()
        .map(|s| s.to_string())
}

/// p2 消息事件字段 → Inbound::Text。非文本消息（text_of 返回 None）丢弃。纯函数。
pub fn to_inbound(
    open_id: &str,
    chat_type: &str,
    message_id: &str,
    parent_id: Option<String>,
    message_type: &str,
    content: &str,
) -> Option<Inbound> {
    Some(Inbound::Text {
        sender_open_id: open_id.to_string(),
        chat_type: chat_type.to_string(),
        message_id: message_id.to_string(),
        parent_id,
        text: text_of(message_type, content)?,
    })
}

/// 起长连接：每条文本消息事件归一化为 Inbound 投递 tx；连接断开返回 Err 由
/// 调用方退避重连。注册失败（builder 报错）也归为 Err。
pub async fn run_ws(
    app_id: String,
    app_secret: String,
    tx: UnboundedSender<Inbound>,
) -> anyhow::Result<()> {
    let client = LarkClient::builder(&app_id, &app_secret)
        .with_app_type(AppType::SelfBuild)
        .with_enable_token_cache(true)
        .build();
    let config = std::sync::Arc::new(client.config.clone());

    let handler = EventDispatcherHandler::builder()
        .register_p2_im_message_receive_v1(move |event| {
            let m = &event.event.message;
            if let Some(inb) = to_inbound(
                &event.event.sender.sender_id.open_id,
                &m.chat_type,
                &m.message_id,
                m.parent_id.clone(),
                &m.message_type,
                &m.content,
            ) {
                let _ = tx.send(inb);
            }
        })
        .map_err(|e| anyhow::anyhow!("feishu ws register receive_v1: {e}"))?
        .build();

    open_lark::client::ws_client::LarkWsClient::open(config, handler)
        .await
        .map_err(|e| anyhow::anyhow!("feishu ws closed: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_of_parses_text_and_rejects_others() {
        assert_eq!(text_of("text", r#"{"text":"允许"}"#).as_deref(), Some("允许"));
        assert_eq!(text_of("image", r#"{"image_key":"k"}"#), None);
        assert_eq!(text_of("text", "not json"), None);
        // text 类型但缺 text 字段 → None（不 panic）
        assert_eq!(text_of("text", r#"{"other":1}"#), None);
    }

    #[test]
    fn to_inbound_maps_fields() {
        let inb = to_inbound("ou_a", "p2p", "om_1", Some("om_0".into()), "text", r#"{"text":"hi"}"#)
            .unwrap();
        assert_eq!(
            inb,
            Inbound::Text {
                sender_open_id: "ou_a".into(),
                chat_type: "p2p".into(),
                message_id: "om_1".into(),
                parent_id: Some("om_0".into()),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn to_inbound_drops_non_text() {
        assert!(to_inbound("ou_a", "p2p", "om_1", None, "image", r#"{"image_key":"k"}"#).is_none());
    }
}
