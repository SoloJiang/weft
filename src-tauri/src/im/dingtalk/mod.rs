//! 钉钉 IM adapter：Stream 模式入站 + Robot OpenAPI 主动单聊/群聊出站。
//!
//! 钉钉 topic-circle thread 通过 `openConvThreadId` 提供稳定路由，但没有
//! 飞书 reply-parent 与可 patch 卡片的完全等价能力：
//! - 在原生 thread 内用 `/bind <issue-id>` 绑定 issue room；
//! - 权限/提问用显式 slash command，应答后另发终态回执；
//! - 收到消息后的短时回复优先 sessionWebhook，过期或失败时回落主动发送。

pub mod ws;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/accessToken";
const PRIVATE_SEND_URL: &str = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend";
const GROUP_SEND_URL: &str = "https://api.dingtalk.com/v1.0/robot/groupMessages/send";
const MAX_TEXT_CHARS: usize = 18_000;
const REPLY_CONTEXT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_REPLY_CONTEXTS: usize = 2_048;

#[derive(Clone)]
struct CachedToken {
    value: String,
    refresh_at: Instant,
}

#[derive(Clone)]
struct ReplyContext {
    session_webhook: String,
    expires_at_ms: i64,
    chat_type: String,
    chat_id: String,
    sender_user_id: String,
    remembered_at: Instant,
}

struct Inner {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    token: tokio::sync::Mutex<Option<CachedToken>>,
    reply_contexts: Mutex<HashMap<String, ReplyContext>>,
    prompt_recipients: Mutex<HashMap<String, String>>,
    recent_inbound: Mutex<HashMap<String, Instant>>,
    robot_code: Mutex<String>,
    local_id: AtomicU64,
    copy: super::outbound::DingTalkCopy,
}

#[derive(Clone)]
pub struct DingTalkChannel {
    inner: Arc<Inner>,
}

impl DingTalkChannel {
    pub fn new(
        client_id: &str,
        client_secret: &str,
        copy: super::outbound::DingTalkCopy,
    ) -> anyhow::Result<Self> {
        let client_id = client_id.trim();
        let client_secret = client_secret.trim();
        if client_id.is_empty() || client_secret.is_empty() {
            anyhow::bail!("dingtalk client id and client secret are required");
        }
        copy.validate()?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| anyhow::anyhow!("dingtalk HTTP client: {e}"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
                http,
                token: tokio::sync::Mutex::new(None),
                reply_contexts: Mutex::new(HashMap::new()),
                prompt_recipients: Mutex::new(HashMap::new()),
                recent_inbound: Mutex::new(HashMap::new()),
                robot_code: Mutex::new(client_id.to_string()),
                local_id: AtomicU64::new(1),
                copy,
            }),
        })
    }

    pub(super) fn credentials(&self) -> (&str, &str) {
        (&self.inner.client_id, &self.inner.client_secret)
    }

    pub(super) fn remember_inbound(&self, message: &ws::RobotMessage) {
        if !message.robot_code.trim().is_empty() {
            *self
                .inner
                .robot_code
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = message.robot_code.trim().to_string();
        }
        if message.session_webhook.trim().is_empty() {
            return;
        }
        let sender_user_id = message.sender_user_id();
        if sender_user_id.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut contexts = self
            .inner
            .reply_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        contexts.retain(|_, context| {
            now.saturating_duration_since(context.remembered_at) < REPLY_CONTEXT_TTL
        });
        if contexts.len() >= MAX_REPLY_CONTEXTS {
            if let Some(oldest_id) = contexts
                .iter()
                .min_by_key(|(_, context)| context.remembered_at)
                .map(|(message_id, _)| message_id.clone())
            {
                contexts.remove(&oldest_id);
            }
        }
        contexts.insert(
            message.msg_id.clone(),
            ReplyContext {
                session_webhook: message.session_webhook.clone(),
                expires_at_ms: message.session_webhook_expired_time,
                chat_type: message.normalized_chat_type().to_string(),
                // If this callback came from a topic-circle thread, delayed
                // fallback must still land inside that thread rather than
                // escape to the parent group.
                chat_id: message.delivery_conversation_id().to_string(),
                sender_user_id,
                remembered_at: now,
            },
        );
    }

    /// Stream 会在未收到 ACK 时重投。五分钟内只把同一 msgId 投给 Weft 一次，
    /// 但 duplicate 仍由 ws 层回 ACK，避免形成永久重试环。
    pub(super) fn mark_inbound_once(&self, message_id: &str) -> bool {
        const TTL: Duration = Duration::from_secs(5 * 60);
        let now = Instant::now();
        let mut recent = self
            .inner
            .recent_inbound
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        recent.retain(|_, seen| now.duration_since(*seen) < TTL);
        if recent.contains_key(message_id) {
            return false;
        }
        recent.insert(message_id.to_string(), now);
        true
    }

    fn robot_code(&self) -> String {
        self.inner
            .robot_code
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn next_local_id(&self) -> String {
        format!(
            "dingtalk-local-{}",
            self.inner.local_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn access_token(&self, force_refresh: bool) -> anyhow::Result<String> {
        let mut cache = self.inner.token.lock().await;
        if !force_refresh {
            if let Some(token) = cache.as_ref() {
                if Instant::now() < token.refresh_at {
                    return Ok(token.value.clone());
                }
            }
        }
        let response = self
            .inner
            .http
            .post(TOKEN_URL)
            .json(&json!({
                "appKey": self.inner.client_id,
                "appSecret": self.inner.client_secret,
            }))
            .send()
            .await
            .map_err(|e| safe_transport_error("token", &e))?;
        let status = response.status();
        let value = response_value(response, "token").await?;
        if !status.is_success() {
            return Err(api_error("token", status, &value));
        }
        let token = value
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("dingtalk token response missing accessToken"))?;
        let expire_in = value
            .get("expireIn")
            .and_then(Value::as_u64)
            .unwrap_or(7_200);
        let refresh_in = expire_in.saturating_sub(300).max(30);
        let value = token.to_string();
        *cache = Some(CachedToken {
            value: value.clone(),
            refresh_at: Instant::now() + Duration::from_secs(refresh_in),
        });
        Ok(value)
    }

    async fn post_openapi(&self, url: &str, body: &Value, scope: &str) -> anyhow::Result<Value> {
        let mut force_refresh = false;
        for attempt in 0..2 {
            let token = self.access_token(force_refresh).await?;
            let response = self
                .inner
                .http
                .post(url)
                .header("x-acs-dingtalk-access-token", token)
                .json(body)
                .send()
                .await
                .map_err(|e| safe_transport_error(scope, &e))?;
            let status = response.status();
            let value = response_value(response, scope).await?;
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                force_refresh = true;
                continue;
            }
            if !status.is_success() {
                return Err(api_error(scope, status, &value));
            }
            return Ok(value);
        }
        anyhow::bail!("dingtalk {scope}: authorization retry exhausted")
    }

    async fn send_private_message(&self, user_id: &str, text: &str) -> anyhow::Result<String> {
        let body = private_message_body(&self.robot_code(), user_id, text);
        let response = self
            .post_openapi(PRIVATE_SEND_URL, &body, "private send")
            .await?;
        Ok(response_message_id(&response).unwrap_or_else(|| self.next_local_id()))
    }

    async fn send_group_message(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        let body = group_message_body(&self.robot_code(), chat_id, text);
        let response = self
            .post_openapi(GROUP_SEND_URL, &body, "group send")
            .await?;
        Ok(response_message_id(&response).unwrap_or_else(|| self.next_local_id()))
    }

    async fn post_session_reply(
        &self,
        message_id: &str,
        context: &ReplyContext,
        text: &str,
    ) -> anyhow::Result<String> {
        let url = validated_session_webhook(&context.session_webhook)?;
        let token = self.access_token(false).await?;
        let response = self
            .inner
            .http
            .post(url)
            .header("x-acs-dingtalk-access-token", token)
            .json(&json!({
                "msgtype": "text",
                "text": { "content": clamp_text(text) },
                "at": { "isAtAll": false, "atUserIds": [context.sender_user_id] }
            }))
            .send()
            .await
            .map_err(|e| safe_transport_error("session reply", &e))?;
        let status = response.status();
        let value = response_value(response, "session reply").await?;
        ensure_session_reply_success(status, &value)?;
        Ok(response_message_id(&value).unwrap_or_else(|| format!("dingtalk-reply-{message_id}")))
    }

    async fn send_reply_fallback(
        &self,
        context: &ReplyContext,
        text: &str,
    ) -> anyhow::Result<String> {
        if context.chat_type == "group" {
            self.send_group_message(&context.chat_id, text).await
        } else {
            self.send_private_message(&context.sender_user_id, text)
                .await
        }
    }

    fn remember_prompt_recipient(&self, message_id: &str, user_id: &str) {
        self.inner
            .prompt_recipients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(message_id.to_string(), user_id.to_string());
    }

    async fn send_prompt_status(&self, message_id: &str, text: &str) -> anyhow::Result<()> {
        let recipient = self
            .inner
            .prompt_recipients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(message_id);
        if let Some(recipient) = recipient {
            self.send_private_message(&recipient, text).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl super::Channel for DingTalkChannel {
    async fn send_card(&self, _open_id: &str, _card: Value) -> anyhow::Result<String> {
        anyhow::bail!("generic interactive cards are unsupported by DingTalk adapter")
    }

    async fn patch_card(&self, _message_id: &str, _card: Value) -> anyhow::Result<()> {
        Ok(())
    }

    async fn send_permission_card(
        &self,
        open_id: &str,
        ask: &crate::ask::Ask,
        _lang: &str,
    ) -> anyhow::Result<String> {
        let message_id = self
            .send_private_message(
                open_id,
                &super::outbound::dingtalk_permission_text(ask, &self.inner.copy),
            )
            .await?;
        self.remember_prompt_recipient(&message_id, open_id);
        Ok(message_id)
    }

    async fn resolve_permission_card(
        &self,
        message_id: &str,
        summary: &str,
        verdict: &str,
        _lang: &str,
    ) -> anyhow::Result<()> {
        self.send_prompt_status(
            message_id,
            &super::outbound::dingtalk_permission_resolved_text(summary, verdict, &self.inner.copy),
        )
        .await
    }

    async fn send_human_question_card(
        &self,
        open_id: &str,
        thread_id: i32,
        ask_id: u64,
        thread_title: &str,
        from: &str,
        text: &str,
        _lang: &str,
    ) -> anyhow::Result<String> {
        let body = super::outbound::dingtalk_human_question_text(
            thread_id,
            ask_id,
            thread_title,
            from,
            text,
            &self.inner.copy,
        );
        let message_id = self.send_private_message(open_id, &body).await?;
        self.remember_prompt_recipient(&message_id, open_id);
        Ok(message_id)
    }

    async fn resolve_human_question_card(
        &self,
        message_id: &str,
        answer: &str,
        _lang: &str,
    ) -> anyhow::Result<()> {
        self.send_prompt_status(
            message_id,
            &super::outbound::dingtalk_human_resolved_text(answer, &self.inner.copy),
        )
        .await
    }

    async fn cancel_human_question_card(
        &self,
        message_id: &str,
        _lang: &str,
    ) -> anyhow::Result<()> {
        self.send_prompt_status(
            message_id,
            super::outbound::dingtalk_human_cancelled_text(&self.inner.copy),
        )
        .await
    }

    fn issue_reply_text(&self, _lang: &str, text: &str) -> String {
        super::outbound::dingtalk_issue_reply_text(&self.inner.copy, text)
    }

    fn resync_summary(&self, _lang: &str, items: &[(i32, String)]) -> String {
        super::outbound::dingtalk_resync_summary(&self.inner.copy, items)
    }

    async fn send_text(&self, open_id: &str, text: &str) -> anyhow::Result<()> {
        self.send_private_message(open_id, text).await.map(|_| ())
    }

    async fn send_chat_text(&self, chat_id: &str, text: &str) -> anyhow::Result<String> {
        self.send_group_message(chat_id, text).await
    }

    async fn create_chat_topic(
        &self,
        _chat_id: &str,
        _seed_message_id: &str,
        _text: &str,
    ) -> anyhow::Result<String> {
        anyhow::bail!(
            "DingTalk thread creation does not return a bindable openConvThreadId through this robot API"
        )
    }

    async fn reply_text(&self, reply_to: &str, text: &str) -> anyhow::Result<String> {
        let context = self
            .inner
            .reply_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(reply_to)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("dingtalk reply context is unavailable"))?;
        if session_webhook_is_fresh(context.expires_at_ms) {
            match self.post_session_reply(reply_to, &context, text).await {
                Ok(message_id) => return Ok(message_id),
                Err(e) => eprintln!("[weft][im] dingtalk session reply fallback: {e}"),
            }
        }
        self.send_reply_fallback(&context, text).await
    }
}

fn clamp_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_TEXT_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_TEXT_CHARS).collect();
    out.push_str("…(truncated)");
    out
}

fn private_message_body(robot_code: &str, user_id: &str, text: &str) -> Value {
    json!({
        "robotCode": robot_code,
        "userIds": [user_id],
        "msgKey": "sampleText",
        "msgParam": json!({ "content": clamp_text(text) }).to_string(),
    })
}

fn group_message_body(robot_code: &str, chat_id: &str, text: &str) -> Value {
    json!({
        "robotCode": robot_code,
        "openConversationId": chat_id,
        "msgKey": "sampleText",
        "msgParam": json!({ "content": clamp_text(text) }).to_string(),
    })
}

fn response_message_id(value: &Value) -> Option<String> {
    ["processQueryKey", "messageId", "outTrackId"]
        .into_iter()
        .find_map(|key| {
            value
                .get(key)
                .or_else(|| value.get("body").and_then(|body| body.get(key)))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

async fn response_value(response: reqwest::Response, scope: &str) -> anyhow::Result<Value> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| safe_transport_error(scope, &e))?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("dingtalk {scope}: invalid JSON response: {e}"))
}

fn safe_transport_error(scope: &str, error: &reqwest::Error) -> anyhow::Error {
    match error.status() {
        Some(status) => anyhow::anyhow!("dingtalk {scope} transport failed with HTTP {status}"),
        None => anyhow::anyhow!("dingtalk {scope} transport failed"),
    }
}

fn api_error(scope: &str, status: reqwest::StatusCode, value: &Value) -> anyhow::Error {
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request rejected");
    anyhow::anyhow!("dingtalk {scope}: HTTP {status}, code={code}, message={message}")
}

fn ensure_session_reply_success(status: reqwest::StatusCode, value: &Value) -> anyhow::Result<()> {
    if !status.is_success() {
        return Err(api_error("session reply", status, value));
    }
    let Some(code) = value.get("errcode") else {
        return Ok(());
    };
    let succeeded = code.as_i64() == Some(0) || code.as_str() == Some("0");
    if succeeded {
        return Ok(());
    }
    let code = code
        .as_i64()
        .map(|value| value.to_string())
        .or_else(|| code.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let message = value
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or("request rejected");
    anyhow::bail!("dingtalk session reply rejected: errcode={code}, errmsg={message}")
}

fn validated_session_webhook(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| anyhow::anyhow!("dingtalk session webhook is invalid"))?;
    let trusted_host = matches!(
        url.host_str(),
        Some("oapi.dingtalk.com") | Some("api.dingtalk.com")
    );
    if url.scheme() != "https" || !trusted_host {
        anyhow::bail!("dingtalk session webhook host is not trusted");
    }
    Ok(url)
}

fn session_webhook_is_fresh(expires_at_ms: i64) -> bool {
    if expires_at_ms <= 0 {
        return true;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(i64::MAX);
    now_ms.saturating_add(5_000) < expires_at_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy() -> super::super::outbound::DingTalkCopy {
        super::super::outbound::DingTalkCopy {
            permission_title: "permission".into(),
            permission_reply_command: "reply".into(),
            verdict_allowed: "allowed".into(),
            verdict_always_allowed: "always".into(),
            verdict_full_access: "full".into(),
            verdict_denied: "denied".into(),
            verdict_expired: "expired".into(),
            verdict_resolved: "resolved".into(),
            human_question_title: "question".into(),
            human_answer_instruction: "answer".into(),
            human_answer_placeholder: "placeholder".into(),
            human_answered: "answered".into(),
            answer_prefix: "answer-prefix".into(),
            human_cancelled: "cancelled".into(),
            issue_not_found: "missing".into(),
            bind_thread_prefix: "bound".into(),
            permission_already_handled: "handled".into(),
            human_already_answered: "already-answered".into(),
            permission_command_usage: "permission-usage".into(),
            human_answer_usage: "answer-usage".into(),
            thread_required: "thread-required".into(),
            free_text_unavailable: "unavailable".into(),
            unbound_thread: "unbound".into(),
            lead_prefix: "lead".into(),
            resync_one: "one".into(),
            resync_many: "many {n}".into(),
            resync_more: "more {n}".into(),
            resync_hint: "hint".into(),
        }
    }

    #[test]
    fn proactive_message_bodies_match_robot_openapi_shape() {
        let private = private_message_body("ding_bot", "user-1", " hello ");
        assert_eq!(private["robotCode"], "ding_bot");
        assert_eq!(private["userIds"], json!(["user-1"]));
        assert_eq!(private["msgKey"], "sampleText");
        assert_eq!(private["msgParam"], r#"{"content":"hello"}"#);

        let group = group_message_body("ding_bot", "cid-1", "hello");
        assert_eq!(group["openConversationId"], "cid-1");
        assert!(group.get("userIds").is_none());
    }

    #[test]
    fn response_id_supports_direct_and_sdk_wrapped_shapes() {
        assert_eq!(
            response_message_id(&json!({"processQueryKey": "q1"})).as_deref(),
            Some("q1")
        );
        assert_eq!(
            response_message_id(&json!({"body": {"processQueryKey": "q2"}})).as_deref(),
            Some("q2")
        );
    }

    #[test]
    fn session_webhook_is_https_and_host_pinned() {
        assert!(validated_session_webhook(
            "https://oapi.dingtalk.com/robot/sendBySession?session=secret"
        )
        .is_ok());
        assert!(validated_session_webhook("http://oapi.dingtalk.com/robot/send").is_err());
        assert!(validated_session_webhook("https://evil.example/robot/send").is_err());
    }

    #[test]
    fn session_reply_rejects_http_200_error_envelope() {
        let error = json!({"errcode": 310000, "errmsg": "session webhook expired"});
        assert!(ensure_session_reply_success(reqwest::StatusCode::OK, &error).is_err());
        assert!(ensure_session_reply_success(
            reqwest::StatusCode::OK,
            &json!({"errcode": 0, "errmsg": "ok"}),
        )
        .is_ok());
    }

    #[test]
    fn text_clamp_is_cjk_safe() {
        let text = "汉".repeat(MAX_TEXT_CHARS + 10);
        let clamped = clamp_text(&text);
        assert!(clamped.starts_with(&"汉".repeat(MAX_TEXT_CHARS)));
        assert!(clamped.ends_with("…(truncated)"));
    }

    #[test]
    fn remembered_thread_reply_falls_back_to_open_conv_thread_id() {
        let channel = DingTalkChannel::new("ding_app", "secret", copy()).unwrap();
        let message = ws::RobotMessage {
            conversation_id: "cid_parent".into(),
            open_conversation_id: "cid_parent".into(),
            open_conv_thread_id: "convThreadEncrypted".into(),
            conversation_type: "2".into(),
            msg_id: "msg_1".into(),
            sender_staff_id: "staff_1".into(),
            sender_id: String::new(),
            session_webhook_expired_time: 0,
            session_webhook: "https://oapi.dingtalk.com/robot/sendBySession?session=x".into(),
            robot_code: "ding_bot".into(),
            msgtype: "text".into(),
            text: ws::TextPayload {
                content: "推进一下".into(),
            },
        };

        channel.remember_inbound(&message);
        let contexts = channel
            .inner
            .reply_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            contexts
                .get("msg_1")
                .map(|context| context.chat_id.as_str()),
            Some("convThreadEncrypted")
        );
    }

    #[test]
    fn inbound_message_ids_are_deduplicated() {
        let channel = DingTalkChannel::new("ding_app", "secret", copy()).unwrap();
        assert!(channel.mark_inbound_once("msg_1"));
        assert!(!channel.mark_inbound_once("msg_1"));
        assert!(channel.mark_inbound_once("msg_2"));
    }

    #[test]
    fn reply_contexts_are_bounded() {
        let channel = DingTalkChannel::new("ding_app", "secret", copy()).unwrap();
        let mut message = ws::RobotMessage {
            conversation_id: "cid_parent".into(),
            open_conversation_id: "cid_parent".into(),
            open_conv_thread_id: "convThreadEncrypted".into(),
            conversation_type: "2".into(),
            sender_staff_id: "staff_1".into(),
            session_webhook: "https://oapi.dingtalk.com/robot/sendBySession?session=x".into(),
            robot_code: "ding_bot".into(),
            msgtype: "text".into(),
            ..ws::RobotMessage::default()
        };

        for index in 0..=MAX_REPLY_CONTEXTS {
            message.msg_id = format!("msg_{index}");
            channel.remember_inbound(&message);
        }

        let contexts = channel
            .inner
            .reply_contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(contexts.len(), MAX_REPLY_CONTEXTS);
        assert!(!contexts.contains_key("msg_0"));
        assert!(contexts.contains_key(&format!("msg_{MAX_REPLY_CONTEXTS}")));
    }
}
