//! 钉钉 Stream Mode wire protocol。协议形态与官方 Node/Go SDK 对齐：
//! 先用 Client ID/Secret 换 endpoint+ticket，再建立 WebSocket；机器人消息走
//! CALLBACK `/v1.0/im/bot/messages/get`，每条都用同 messageId 回 ACK。

use crate::im::inbound::Inbound;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

const GATEWAY_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const ROBOT_TOPIC: &str = "/v1.0/im/bot/messages/get";

#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RobotMessage {
    pub conversation_id: String,
    /// Parent group id when DingTalk supplies it separately from a topic-circle
    /// thread id. Older callbacks omit it and use `conversationId` alone.
    #[serde(alias = "parentConversationId")]
    pub open_conversation_id: String,
    /// Stable topic-circle/thread identity. DingTalk surfaces several names
    /// across API generations; preserve all observed aliases at the boundary.
    #[serde(
        alias = "openConversationThreadId",
        alias = "threadId",
        alias = "topicId"
    )]
    pub open_conv_thread_id: String,
    pub conversation_type: String,
    pub msg_id: String,
    pub sender_staff_id: String,
    pub sender_id: String,
    pub session_webhook_expired_time: i64,
    pub session_webhook: String,
    pub robot_code: String,
    pub msgtype: String,
    pub text: TextPayload,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct TextPayload {
    pub content: String,
}

impl RobotMessage {
    pub fn sender_user_id(&self) -> String {
        if self.sender_staff_id.trim().is_empty() {
            self.sender_id.trim().to_string()
        } else {
            self.sender_staff_id.trim().to_string()
        }
    }

    pub fn normalized_chat_type(&self) -> Option<&'static str> {
        match self.conversation_type.trim().to_ascii_lowercase().as_str() {
            "1" | "p2p" => Some("p2p"),
            "2" | "group" => Some("group"),
            _ => None,
        }
    }

    pub fn group_conversation_id(&self) -> &str {
        if self.open_conversation_id.trim().is_empty() {
            self.conversation_id.trim()
        } else {
            self.open_conversation_id.trim()
        }
    }

    pub fn thread_id(&self) -> Option<&str> {
        let explicit = self.open_conv_thread_id.trim();
        if !explicit.is_empty() {
            return Some(explicit);
        }
        let conversation_id = self.conversation_id.trim();
        conversation_id
            .starts_with("convThread")
            .then_some(conversation_id)
    }

    pub fn delivery_conversation_id(&self) -> &str {
        self.thread_id()
            .unwrap_or_else(|| self.group_conversation_id())
    }
}

#[derive(Deserialize)]
struct ConnectionResponse {
    endpoint: String,
    ticket: String,
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct DownstreamHeaders {
    content_type: String,
    message_id: String,
    topic: String,
}

impl Default for DownstreamHeaders {
    fn default() -> Self {
        Self {
            content_type: "application/json".to_string(),
            message_id: String::new(),
            topic: String::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Downstream {
    r#type: String,
    headers: DownstreamHeaders,
    data: String,
}

impl Default for Downstream {
    fn default() -> Self {
        Self {
            r#type: String::new(),
            headers: DownstreamHeaders::default(),
            data: String::new(),
        }
    }
}

enum FrameOutcome {
    Continue,
    Disconnect,
}

pub fn to_inbound(message: &RobotMessage) -> Option<Inbound> {
    if message.msgtype != "text" {
        return None;
    }
    let sender_open_id = message.sender_user_id();
    let chat_id = message.group_conversation_id();
    let message_id = message.msg_id.trim();
    let text = message.text.content.trim();
    if sender_open_id.is_empty() || chat_id.is_empty() || message_id.is_empty() || text.is_empty() {
        return None;
    }
    let chat_type = message.normalized_chat_type()?;
    let thread_id = if chat_type == "group" {
        message.thread_id().map(str::to_string)
    } else {
        None
    };
    Some(Inbound::Text {
        sender_open_id,
        chat_type: chat_type.to_string(),
        chat_id: chat_id.to_string(),
        thread_id,
        message_id: message_id.to_string(),
        parent_id: None,
        text: text.to_string(),
    })
}

pub async fn run_ws(
    channel: Arc<super::DingTalkChannel>,
    tx: UnboundedSender<Inbound>,
) -> anyhow::Result<()> {
    let (client_id, client_secret) = channel.credentials();
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("dingtalk gateway HTTP client: {e}"))?
        .post(GATEWAY_URL)
        .header("Accept", "application/json")
        .json(&json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "ua": "weft/0.1",
            "subscriptions": [{ "type": "CALLBACK", "topic": ROBOT_TOPIC }]
        }))
        .send()
        .await
        .map_err(|e| super::safe_transport_error("gateway", &e))?;
    let status = response.status();
    let value = super::response_value(response, "gateway").await?;
    if !status.is_success() {
        return Err(super::api_error("gateway", status, &value));
    }
    let connection: ConnectionResponse = serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("dingtalk gateway response: {e}"))?;
    if connection.endpoint.trim().is_empty() || connection.ticket.trim().is_empty() {
        anyhow::bail!("dingtalk gateway response missing endpoint or ticket");
    }
    let mut endpoint = reqwest::Url::parse(&connection.endpoint)
        .map_err(|_| anyhow::anyhow!("dingtalk gateway returned an invalid endpoint"))?;
    if endpoint.scheme() != "wss" {
        anyhow::bail!("dingtalk gateway endpoint must use wss");
    }
    endpoint
        .query_pairs_mut()
        .append_pair("ticket", &connection.ticket);

    let (mut socket, _) = tokio_tungstenite::connect_async(endpoint.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("dingtalk websocket connect failed: {e}"))?;
    eprintln!("[weft][im] dingtalk stream connected");
    while let Some(frame) = socket.next().await {
        let frame = frame.map_err(|e| anyhow::anyhow!("dingtalk websocket read: {e}"))?;
        match frame {
            Message::Text(text) => {
                let (ack, outcome) = handle_downstream(&channel, &tx, text.as_str())?;
                if let Some(ack) = ack {
                    socket
                        .send(Message::Text(ack.to_string().into()))
                        .await
                        .map_err(|e| anyhow::anyhow!("dingtalk websocket ACK: {e}"))?;
                }
                if matches!(outcome, FrameOutcome::Disconnect) {
                    return Ok(());
                }
            }
            Message::Binary(bytes) => {
                let text = std::str::from_utf8(bytes.as_ref())
                    .map_err(|_| anyhow::anyhow!("dingtalk websocket sent non-UTF8 data"))?;
                let (ack, outcome) = handle_downstream(&channel, &tx, text)?;
                if let Some(ack) = ack {
                    socket
                        .send(Message::Text(ack.to_string().into()))
                        .await
                        .map_err(|e| anyhow::anyhow!("dingtalk websocket ACK: {e}"))?;
                }
                if matches!(outcome, FrameOutcome::Disconnect) {
                    return Ok(());
                }
            }
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|e| anyhow::anyhow!("dingtalk websocket pong: {e}"))?;
            }
            Message::Close(_) => return Ok(()),
            Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    anyhow::bail!("dingtalk websocket closed")
}

fn handle_downstream(
    channel: &super::DingTalkChannel,
    tx: &UnboundedSender<Inbound>,
    raw: &str,
) -> anyhow::Result<(Option<Value>, FrameOutcome)> {
    let frame: Downstream =
        serde_json::from_str(raw).map_err(|e| anyhow::anyhow!("dingtalk downstream JSON: {e}"))?;
    if frame.r#type == "SYSTEM" {
        return match frame.headers.topic.as_str() {
            "ping" => Ok((
                Some(system_ack(&frame.headers.message_id, &frame.data)),
                FrameOutcome::Continue,
            )),
            "disconnect" => Ok((
                Some(system_ack(&frame.headers.message_id, &frame.data)),
                FrameOutcome::Disconnect,
            )),
            _ => Ok((None, FrameOutcome::Continue)),
        };
    }
    if frame.r#type != "CALLBACK" || frame.headers.topic != ROBOT_TOPIC {
        return Ok((
            Some(error_ack(
                &frame.headers.message_id,
                404,
                "unsupported topic",
            )),
            FrameOutcome::Continue,
        ));
    }
    if frame.headers.message_id.is_empty() {
        anyhow::bail!("dingtalk callback missing stream messageId");
    }
    let robot: RobotMessage = match serde_json::from_str(&frame.data) {
        Ok(message) => message,
        Err(_) => {
            return Ok((
                Some(error_ack(
                    &frame.headers.message_id,
                    400,
                    "invalid robot payload",
                )),
                FrameOutcome::Continue,
            ));
        }
    };
    if let Some(inbound) = to_inbound(&robot) {
        channel.remember_inbound(&robot);
        if channel.mark_inbound_once(&robot.msg_id) {
            tx.send(inbound)
                .map_err(|_| anyhow::anyhow!("dingtalk inbound receiver closed"))?;
        }
    }
    Ok((
        Some(callback_ack(&frame.headers.message_id)),
        FrameOutcome::Continue,
    ))
}

fn callback_ack(message_id: &str) -> Value {
    json!({
        "code": 200,
        "headers": { "contentType": "application/json", "messageId": message_id },
        "message": "OK",
        "data": json!({ "response": Value::Null }).to_string(),
    })
}

fn system_ack(message_id: &str, data: &str) -> Value {
    json!({
        "code": 200,
        "headers": { "contentType": "application/json", "messageId": message_id },
        "message": "OK",
        "data": data,
    })
}

fn error_ack(message_id: &str, code: u16, message: &str) -> Value {
    json!({
        "code": code,
        "headers": { "contentType": "application/json", "messageId": message_id },
        "message": message,
        "data": "",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_message(conversation_type: &str) -> RobotMessage {
        RobotMessage {
            conversation_id: "cid_group".into(),
            open_conversation_id: String::new(),
            open_conv_thread_id: String::new(),
            conversation_type: conversation_type.into(),
            msg_id: "msg_1".into(),
            sender_staff_id: "staff_1".into(),
            sender_id: "sender_fallback".into(),
            session_webhook_expired_time: 0,
            session_webhook: "https://oapi.dingtalk.com/robot/sendBySession?session=x".into(),
            robot_code: "ding_bot".into(),
            msgtype: "text".into(),
            text: TextPayload {
                content: " 推一下 ".into(),
            },
        }
    }

    #[test]
    fn group_text_uses_real_open_conv_thread_id_as_route_ref() {
        let mut message = text_message("2");
        message.open_conv_thread_id = "convThreadEncrypted".into();
        assert_eq!(
            to_inbound(&message),
            Some(Inbound::Text {
                sender_open_id: "staff_1".into(),
                chat_type: "group".into(),
                chat_id: "cid_group".into(),
                thread_id: Some("convThreadEncrypted".into()),
                message_id: "msg_1".into(),
                parent_id: None,
                text: "推一下".into(),
            })
        );
    }

    #[test]
    fn ordinary_group_does_not_fake_a_thread() {
        let inbound = to_inbound(&text_message("2"));
        assert!(matches!(
            inbound,
            Some(Inbound::Text {
                thread_id: None,
                ..
            })
        ));
    }

    #[test]
    fn legacy_thread_shaped_conversation_id_is_preserved() {
        let mut message = text_message("2");
        message.conversation_id = "convThreadLegacy".into();
        assert_eq!(message.thread_id(), Some("convThreadLegacy"));
        assert_eq!(message.delivery_conversation_id(), "convThreadLegacy");
    }

    #[test]
    fn callback_deserializes_parent_group_and_thread_aliases() {
        let message: RobotMessage = serde_json::from_value(json!({
            "conversationId": "convThreadEncrypted",
            "openConversationId": "cid_parent_group",
            "openConversationThreadId": "convThreadEncrypted",
            "conversationType": "2",
            "msgId": "msg_1",
            "senderStaffId": "staff_1",
            "msgtype": "text",
            "text": { "content": "推进一下" }
        }))
        .unwrap();

        assert_eq!(message.group_conversation_id(), "cid_parent_group");
        assert_eq!(message.thread_id(), Some("convThreadEncrypted"));
        assert_eq!(message.delivery_conversation_id(), "convThreadEncrypted");
        assert!(matches!(
            to_inbound(&message),
            Some(Inbound::Text {
                chat_id,
                thread_id: Some(thread_id),
                ..
            }) if chat_id == "cid_parent_group" && thread_id == "convThreadEncrypted"
        ));
    }

    #[test]
    fn p2p_text_uses_sender_fallback_and_has_no_topic_ref() {
        let mut message = text_message("1");
        message.sender_staff_id.clear();
        assert_eq!(
            to_inbound(&message),
            Some(Inbound::Text {
                sender_open_id: "sender_fallback".into(),
                chat_type: "p2p".into(),
                chat_id: "cid_group".into(),
                thread_id: None,
                message_id: "msg_1".into(),
                parent_id: None,
                text: "推一下".into(),
            })
        );
    }

    #[test]
    fn non_text_and_incomplete_messages_are_dropped() {
        let mut message = text_message("1");
        message.msgtype = "picture".into();
        assert!(to_inbound(&message).is_none());
        message.msgtype = "text".into();
        message.msg_id.clear();
        assert!(to_inbound(&message).is_none());
    }

    #[test]
    fn missing_and_unknown_conversation_types_are_dropped() {
        let missing: RobotMessage = serde_json::from_value(json!({
            "conversationId": "cid_group",
            "msgId": "msg_1",
            "senderStaffId": "staff_1",
            "msgtype": "text",
            "text": { "content": "推进一下" }
        }))
        .unwrap();
        assert!(to_inbound(&missing).is_none());

        let unknown = text_message("unexpected");
        assert!(to_inbound(&unknown).is_none());
    }

    #[test]
    fn callback_ack_uses_stream_message_id_and_response_wrapper() {
        let ack = callback_ack("stream_1");
        assert_eq!(ack["code"], 200);
        assert_eq!(ack["headers"]["messageId"], "stream_1");
        assert_eq!(
            serde_json::from_str::<Value>(ack["data"].as_str().unwrap_or_default()).unwrap(),
            json!({"response": null})
        );
    }
}
