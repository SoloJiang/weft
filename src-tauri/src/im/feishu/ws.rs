//! 飞书长连接：事件 → 归一化 Inbound。openlark 0.17 起 `EventDispatcherHandler` 只提供
//! `register_raw`（无 0.14 的 `register_p2_im_message_receive_v1` 强类型注册），故入站走
//! 「原始 envelope 字节 → 解析 → Inbound」：`inbound_from_raw` 是纯函数，单测覆盖。
//! 启动以 0.17 实测 API：`LarkWsClient::open(Arc<Config>, EventDispatcherHandler)`，阻塞到
//! 连接结束；断线由外层循环指数退避重连。只关心 `im.message.receive_v1`（其余事件丢弃）。

use crate::im::inbound::Inbound;
use open_lark::ws_client::{EventDispatcherHandler, EventHandler, LarkWsClient};
use tokio::sync::mpsc::UnboundedSender;

/// "text" 消息的 content 是 {"text":"..."}；其余类型不收。纯函数。
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
    chat_id: &str,
    thread_id: Option<String>,
    message_id: &str,
    parent_id: Option<String>,
    message_type: &str,
    content: &str,
) -> Option<Inbound> {
    Some(Inbound::Text {
        sender_open_id: open_id.to_string(),
        chat_type: chat_type.to_string(),
        chat_id: chat_id.to_string(),
        thread_id,
        message_id: message_id.to_string(),
        parent_id,
        text: text_of(message_type, content)?,
    })
}

/// 原始事件 envelope（`{header:{event_type}, event:{sender, message}}`）→ Inbound。
/// 非 `im.message.receive_v1` 事件、缺字段、或非文本消息都返回 None（不 panic）。纯函数。
pub fn inbound_from_raw(env: &serde_json::Value) -> Option<Inbound> {
    if env.get("header")?.get("event_type")?.as_str()? != "im.message.receive_v1" {
        return None;
    }
    let e = env.get("event")?;
    let m = e.get("message")?;
    to_inbound(
        e.get("sender")?
            .get("sender_id")?
            .get("open_id")?
            .as_str()?,
        m.get("chat_type")?.as_str()?,
        m.get("chat_id")?.as_str()?,
        m.get("thread_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        m.get("message_id")?.as_str()?,
        m.get("parent_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        m.get("message_type")?.as_str()?,
        m.get("content")?.as_str()?,
    )
}

/// register_raw 的事件处理器：拿到原始 envelope 字节，解析成 Inbound 后投递 tx。
/// `EventHandler::handle` 是 `&self`，故把 sender 存进结构体（Send + Sync）。
struct RawInbound {
    tx: UnboundedSender<Inbound>,
}

impl EventHandler for RawInbound {
    fn handle(&self, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 解析失败（非 JSON / 非目标事件）静默丢弃——长连接还会送别的事件，不该报错断流。
        if let Ok(env) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(inb) = inbound_from_raw(&env) {
                eprintln!("[weft][im] inbound feishu text (raw event)");
                let _ = self.tx.send(inb);
            }
        }
        Ok(())
    }
}

/// 起长连接：每条文本消息事件归一化为 Inbound 投递 tx；连接断开返回 Err 由
/// 调用方退避重连。注册失败（register_raw 报错）也归为 Err。
// `open_lark::Config`（openlark_client）在 0.17 标记 deprecated（将并入 openlark_core），
// 但 `LarkWsClient::open` 的签名仍只收这个类型，故此处是唯一合法路径——并入完成前
// 局部 allow，不让一条过渡期告警污染整库。
#[allow(deprecated)]
pub async fn run_ws(
    app_id: String,
    app_secret: String,
    tx: UnboundedSender<Inbound>,
) -> anyhow::Result<()> {
    // 长连接用 openlark_client 的 `Config`（区别于 REST 的 CoreConfig）：
    // `LarkWsClient::open` 要 `Arc<openlark_client::config::Config>`。
    let config = open_lark::Config::builder()
        .app_id(&app_id)
        .app_secret(&app_secret)
        .enable_token_cache(true)
        .build()
        .map_err(|e| anyhow::anyhow!("feishu ws config: {e}"))?;
    let config = std::sync::Arc::new(config);

    let handler = EventDispatcherHandler::builder()
        .register_raw(EventDispatcherHandler::RAW_EVENT_KEY, RawInbound { tx })
        .map_err(|e| anyhow::anyhow!("feishu ws register raw: {e}"))?
        .build();

    LarkWsClient::open(config, handler)
        .await
        .map_err(|e| anyhow::anyhow!("feishu ws closed: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_of_parses_text_and_rejects_others() {
        assert_eq!(
            text_of("text", r#"{"text":"允许"}"#).as_deref(),
            Some("允许")
        );
        assert_eq!(text_of("image", r#"{"image_key":"k"}"#), None);
        assert_eq!(text_of("text", "not json"), None);
        // text 类型但缺 text 字段 → None（不 panic）
        assert_eq!(text_of("text", r#"{"other":1}"#), None);
    }

    #[test]
    fn to_inbound_maps_fields() {
        let inb = to_inbound(
            "ou_a",
            "p2p",
            "oc_dm",
            None,
            "om_1",
            Some("om_0".into()),
            "text",
            r#"{"text":"hi"}"#,
        )
        .unwrap();
        assert_eq!(
            inb,
            Inbound::Text {
                sender_open_id: "ou_a".into(),
                chat_type: "p2p".into(),
                chat_id: "oc_dm".into(),
                thread_id: None,
                message_id: "om_1".into(),
                parent_id: Some("om_0".into()),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn to_inbound_carries_group_thread_id() {
        let inb = to_inbound(
            "ou_a",
            "group",
            "oc_g",
            Some("omt_42".into()),
            "om_1",
            None,
            "text",
            r#"{"text":"推一下"}"#,
        )
        .unwrap();
        assert_eq!(
            inb,
            Inbound::Text {
                sender_open_id: "ou_a".into(),
                chat_type: "group".into(),
                chat_id: "oc_g".into(),
                thread_id: Some("omt_42".into()),
                message_id: "om_1".into(),
                parent_id: None,
                text: "推一下".into(),
            }
        );
    }

    #[test]
    fn to_inbound_drops_non_text() {
        assert!(to_inbound(
            "ou_a",
            "p2p",
            "oc_dm",
            None,
            "om_1",
            None,
            "image",
            r#"{"image_key":"k"}"#
        )
        .is_none());
    }

    #[test]
    fn inbound_from_raw_extracts_group_fields() {
        let env = serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_a"}},
                "message": {
                    "chat_type": "group", "chat_id": "oc_g", "thread_id": "omt_42",
                    "message_id": "om_1", "message_type": "text",
                    "content": r#"{"text":"推一下"}"#
                }
            }
        });
        assert_eq!(
            inbound_from_raw(&env).unwrap(),
            Inbound::Text {
                sender_open_id: "ou_a".into(),
                chat_type: "group".into(),
                chat_id: "oc_g".into(),
                thread_id: Some("omt_42".into()),
                message_id: "om_1".into(),
                parent_id: None,
                text: "推一下".into(),
            }
        );
    }

    #[test]
    fn inbound_from_raw_carries_parent_id_in_p2p() {
        let env = serde_json::json!({
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_a"}},
                "message": {
                    "chat_type": "p2p", "chat_id": "oc_dm",
                    "message_id": "om_1", "parent_id": "om_0", "message_type": "text",
                    "content": r#"{"text":"hi"}"#
                }
            }
        });
        assert_eq!(
            inbound_from_raw(&env).unwrap(),
            Inbound::Text {
                sender_open_id: "ou_a".into(),
                chat_type: "p2p".into(),
                chat_id: "oc_dm".into(),
                thread_id: None,
                message_id: "om_1".into(),
                parent_id: Some("om_0".into()),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn inbound_from_raw_drops_non_message_events() {
        let env = serde_json::json!({"header": {"event_type": "im.chat.updated_v1"}, "event": {}});
        assert!(inbound_from_raw(&env).is_none());
    }

    #[test]
    fn inbound_from_raw_drops_malformed_envelope() {
        // 缺 header / 缺 event / 缺字段都返回 None，不 panic。
        assert!(inbound_from_raw(&serde_json::json!({})).is_none());
        assert!(inbound_from_raw(
            &serde_json::json!({"header": {"event_type": "im.message.receive_v1"}})
        )
        .is_none());
    }
}
