//! 飞书扫码接入:device-flow 应用注册(RFC 8628,against accounts.feishu.cn）。
//!
//! 协议端点 `POST /oauth/v1/app/registration`(form):
//!  - begin:`action=begin & archetype=PersonalAgent & auth_method=client_secret &
//!    request_user_info=open_id`（无 client_id，故本地可独立发起）→ 返回 device_code +
//!    verification_uri_complete（二维码目标）+ interval + expire_in。
//!  - poll:`action=poll & device_code=…` → 成功返回 {client_id, client_secret,
//!    user_info.open_id}（即 app 凭证）；authorization_pending / slow_down /
//!    access_denied / expired_token；user_info.tenant_brand=="lark" 表示国际版账号。
//!
//! 解析(`parse_begin`)与状态机(`classify_poll`)是纯函数、单测覆盖;HTTP 传输与后台
//! 轮询见本文件后半（RegistrationService）。MVP 仅飞书国内:遇 tenant_brand=lark 报错。

/// device-flow 默认轮询/过期参数（服务端省略时回落，与官方 SDK 一致）。
pub const FEISHU_DOMAIN: &str = "https://accounts.feishu.cn";
pub const REG_ENDPOINT: &str = "/oauth/v1/app/registration";
pub const DEFAULT_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_EXPIRE_SECS: u64 = 600;

/// begin 阶段的归一化结果。
#[derive(Debug, Clone, PartialEq)]
pub struct BeginResult {
    pub device_code: String,
    pub verification_uri_complete: String,
    pub interval_secs: u64,
    pub expire_secs: u64,
}

/// 一次 poll 的归类结果——状态机的一步。
#[derive(Debug, Clone, PartialEq)]
pub enum PollOutcome {
    /// 等待用户扫码/授权,继续轮询。
    Pending,
    /// 服务端要求放慢:间隔 +5s 后继续。
    SlowDown,
    /// 用户用国际版 Lark 账号授权;MVP 不支持。
    LarkUnsupported,
    /// 终态成功:拿到应用凭证。
    Success {
        client_id: String,
        client_secret: String,
        open_id: String,
    },
    /// 用户拒绝授权。
    Denied,
    /// device_code 过期。
    Expired,
    /// 其它协议错误码。
    Protocol { code: String, desc: String },
}

/// 解析 begin 响应。缺 device_code / verification_uri_complete 视为协议错误;
/// interval / expire_in 缺失或非正回落到默认值。
pub fn parse_begin(v: &serde_json::Value) -> anyhow::Result<BeginResult> {
    let device_code = v
        .get("device_code")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("registration begin: missing device_code"))?
        .to_string();
    let verification_uri_complete = v
        .get("verification_uri_complete")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("registration begin: missing verification_uri_complete"))?
        .to_string();
    let interval_secs = v
        .get("interval")
        .and_then(|x| x.as_u64())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let expire_secs = v
        .get("expire_in")
        .and_then(|x| x.as_u64())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_EXPIRE_SECS);
    Ok(BeginResult {
        device_code,
        verification_uri_complete,
        interval_secs,
        expire_secs,
    })
}

/// 把一次 poll 响应归类为状态机的一步。
pub fn classify_poll(v: &serde_json::Value) -> PollOutcome {
    // lark 租户优先判定:MVP 仅飞书国内,即便带凭证也归为不支持。
    if v.get("user_info")
        .and_then(|u| u.get("tenant_brand"))
        .and_then(|b| b.as_str())
        == Some("lark")
    {
        return PollOutcome::LarkUnsupported;
    }
    let client_id = v.get("client_id").and_then(|x| x.as_str()).unwrap_or("");
    let client_secret = v.get("client_secret").and_then(|x| x.as_str()).unwrap_or("");
    if !client_id.is_empty() && !client_secret.is_empty() {
        let open_id = v
            .get("user_info")
            .and_then(|u| u.get("open_id"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        return PollOutcome::Success {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            open_id,
        };
    }
    match v.get("error").and_then(|x| x.as_str()).unwrap_or("") {
        "authorization_pending" | "" => PollOutcome::Pending,
        "slow_down" => PollOutcome::SlowDown,
        "access_denied" => PollOutcome::Denied,
        "expired_token" => PollOutcome::Expired,
        other => PollOutcome::Protocol {
            code: other.to_string(),
            desc: v
                .get("error_description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_begin_reads_fields() {
        let v = serde_json::json!({
            "device_code": "dc",
            "verification_uri_complete": "https://x?q=1",
            "interval": 6,
            "expire_in": 300
        });
        let b = parse_begin(&v).unwrap();
        assert_eq!(b.device_code, "dc");
        assert_eq!(b.verification_uri_complete, "https://x?q=1");
        assert_eq!(b.interval_secs, 6);
        assert_eq!(b.expire_secs, 300);
    }

    #[test]
    fn parse_begin_defaults_interval_and_expire() {
        let v = serde_json::json!({"device_code": "dc", "verification_uri_complete": "u"});
        let b = parse_begin(&v).unwrap();
        assert_eq!(b.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(b.expire_secs, DEFAULT_EXPIRE_SECS);
    }

    #[test]
    fn parse_begin_errors_without_device_code() {
        assert!(parse_begin(&serde_json::json!({"verification_uri_complete": "u"})).is_err());
    }

    #[test]
    fn parse_begin_errors_without_verification_uri() {
        assert!(parse_begin(&serde_json::json!({"device_code": "dc"})).is_err());
    }

    #[test]
    fn classify_success_extracts_creds_and_open_id() {
        let v = serde_json::json!({
            "client_id": "cli_x",
            "client_secret": "sec",
            "user_info": {"open_id": "ou_1"}
        });
        assert_eq!(
            classify_poll(&v),
            PollOutcome::Success {
                client_id: "cli_x".into(),
                client_secret: "sec".into(),
                open_id: "ou_1".into()
            }
        );
    }

    #[test]
    fn classify_lark_tenant_is_unsupported() {
        // 即便同时带 client_id,lark 租户也优先判为不支持(MVP)。
        let v = serde_json::json!({
            "client_id": "cli_x",
            "client_secret": "sec",
            "user_info": {"tenant_brand": "lark", "open_id": "ou_1"}
        });
        assert_eq!(classify_poll(&v), PollOutcome::LarkUnsupported);
    }

    #[test]
    fn classify_pending() {
        assert_eq!(
            classify_poll(&serde_json::json!({"error": "authorization_pending"})),
            PollOutcome::Pending
        );
    }

    #[test]
    fn classify_slow_down() {
        assert_eq!(
            classify_poll(&serde_json::json!({"error": "slow_down"})),
            PollOutcome::SlowDown
        );
    }

    #[test]
    fn classify_denied() {
        assert_eq!(
            classify_poll(&serde_json::json!({"error": "access_denied"})),
            PollOutcome::Denied
        );
    }

    #[test]
    fn classify_expired() {
        assert_eq!(
            classify_poll(&serde_json::json!({"error": "expired_token"})),
            PollOutcome::Expired
        );
    }

    #[test]
    fn classify_unknown_error_is_protocol() {
        assert_eq!(
            classify_poll(&serde_json::json!({"error": "weird", "error_description": "d"})),
            PollOutcome::Protocol {
                code: "weird".into(),
                desc: "d".into()
            }
        );
    }

    #[test]
    fn classify_empty_is_pending() {
        assert_eq!(classify_poll(&serde_json::json!({})), PollOutcome::Pending);
    }
}
