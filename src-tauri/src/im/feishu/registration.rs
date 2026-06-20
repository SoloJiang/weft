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

use std::sync::{Arc, Mutex};

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

/// 把 begin 返回的 verification_uri_complete 拼成最终二维码 URL:附 `source=weft`
/// 便于飞书侧归因、`app_name=Weft` 预填创建页名称。已有 query 用 `&`,否则用 `?`。
pub fn build_qr_url(verification_uri_complete: &str) -> String {
    let sep = if verification_uri_complete.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{verification_uri_complete}{sep}source=weft&app_name=Weft")
}

/// 由内容(二维码 URL)生成 SVG,转 `data:image/svg+xml;base64,…`。img 标签加载的
/// SVG 不执行脚本,前端可安全 `<img src>` 渲染。
pub fn qr_svg_data_uri(content: &str) -> anyhow::Result<String> {
    use base64::Engine;
    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|e| anyhow::anyhow!("qr encode: {e}"))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .build();
    let b64 = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
    Ok(format!("data:image/svg+xml;base64,{b64}"))
}

// ───────────────────────── HTTP 传输 ─────────────────────────

/// device-flow 的 HTTP 传输面。抽成 trait 以便单测注入 fake,生产用 reqwest。
#[async_trait::async_trait]
pub trait RegistrationTransport: Send + Sync {
    async fn post_form(&self, url: &str, form: &[(&str, &str)])
        -> anyhow::Result<serde_json::Value>;
}

/// 生产传输:reqwest 发 x-www-form-urlencoded POST,解析 JSON 响应。
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl RegistrationTransport for ReqwestTransport {
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .client
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("registration request: {e}"))?;
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| anyhow::anyhow!("registration response json: {e}"))
    }
}

// ───────────────────────── 注册 session / 服务 ─────────────────────────

/// 扫码会话状态(供前端轮询展示;**不含** device_code / 凭证)。
#[derive(Debug, Clone, PartialEq)]
pub enum ScanStatus {
    Idle,
    Pending,
    Success,
    Expired,
    Error(String),
}

/// begin 成功后回给前端的视图。
pub struct ScanBegin {
    pub qr_data_uri: String,
    pub interval_secs: u64,
    pub expire_secs: u64,
}

/// 成功落库回调:拿到凭证后写库 + 重连。返回 future 以支持 async 落库。
pub type OnSuccess = Arc<
    dyn Fn(String, String, String) -> futures::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync,
>;

struct Session {
    status: ScanStatus,
    generation: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            status: ScanStatus::Idle,
            generation: 0,
        }
    }
}

/// 后台轮询的扫码注册服务(挂为 Tauri managed state)。同一时刻只跑一代:
/// 再次 begin 或 cancel 自增代际号,旧轮询 task 检查到代际变化即退出。
#[derive(Default)]
pub struct RegistrationService {
    session: Arc<Mutex<Session>>,
}

impl RegistrationService {
    pub fn status(&self) -> ScanStatus {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    pub fn cancel(&self) {
        let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        s.generation = s.generation.wrapping_add(1);
        s.status = ScanStatus::Idle;
    }

    /// 发起 device-flow:begin → 二维码 → 标 Pending → spawn 后台轮询。
    pub async fn begin(
        &self,
        transport: Arc<dyn RegistrationTransport>,
        on_success: OnSuccess,
    ) -> anyhow::Result<ScanBegin> {
        let (view, device_code, interval, expire) = do_begin(transport.as_ref()).await?;
        let generation = {
            let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
            s.generation = s.generation.wrapping_add(1);
            s.status = ScanStatus::Pending;
            s.generation
        };
        let session = self.session.clone();
        tauri::async_runtime::spawn(async move {
            poll_loop(
                transport, session, generation, device_code, interval, expire, on_success,
            )
            .await;
        });
        Ok(view)
    }
}

fn reg_url() -> String {
    format!("{FEISHU_DOMAIN}{REG_ENDPOINT}")
}

fn set_status(session: &Arc<Mutex<Session>>, generation: u64, status: ScanStatus) {
    let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
    if s.generation == generation {
        s.status = status;
    }
}

fn is_live(session: &Arc<Mutex<Session>>, generation: u64) -> bool {
    session.lock().unwrap_or_else(|e| e.into_inner()).generation == generation
}

/// begin 请求 + 解析 + 二维码。返回前端视图 + 轮询所需的 device_code/interval/expire。
async fn do_begin(
    transport: &dyn RegistrationTransport,
) -> anyhow::Result<(ScanBegin, String, u64, u64)> {
    let resp = transport
        .post_form(
            &reg_url(),
            &[
                ("action", "begin"),
                ("archetype", "PersonalAgent"),
                ("auth_method", "client_secret"),
                ("request_user_info", "open_id"),
            ],
        )
        .await?;
    let begin = parse_begin(&resp)?;
    let qr_url = build_qr_url(&begin.verification_uri_complete);
    let qr_data_uri = qr_svg_data_uri(&qr_url)?;
    Ok((
        ScanBegin {
            qr_data_uri,
            interval_secs: begin.interval_secs,
            expire_secs: begin.expire_secs,
        },
        begin.device_code,
        begin.interval_secs,
        begin.expire_secs,
    ))
}

/// 轮询循环:先 poll、必要时再 sleep。终态写 status 后退出;代际变化即退出。
async fn poll_loop(
    transport: Arc<dyn RegistrationTransport>,
    session: Arc<Mutex<Session>>,
    generation: u64,
    device_code: String,
    interval_secs: u64,
    expire_secs: u64,
    on_success: OnSuccess,
) {
    let url = reg_url();
    let mut interval = interval_secs;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(expire_secs);
    loop {
        if !is_live(&session, generation) {
            return; // 被 cancel 或新一代取代。
        }
        if tokio::time::Instant::now() >= deadline {
            set_status(&session, generation, ScanStatus::Expired);
            return;
        }
        let resp = match transport
            .post_form(
                &url,
                &[("action", "poll"), ("device_code", device_code.as_str())],
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // 网络抖动不致命:记录后按当前间隔重试。
                eprintln!("[weft][im] scan poll request: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
                continue;
            }
        };
        match classify_poll(&resp) {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += 5,
            PollOutcome::Success {
                client_id,
                client_secret,
                open_id,
            } => {
                match on_success(client_id, client_secret, open_id).await {
                    Ok(()) => set_status(&session, generation, ScanStatus::Success),
                    Err(e) => {
                        eprintln!("[weft][im] scan apply credentials: {e}");
                        set_status(&session, generation, ScanStatus::Error("apply_failed".into()));
                    }
                }
                return;
            }
            PollOutcome::LarkUnsupported => {
                set_status(
                    &session,
                    generation,
                    ScanStatus::Error("lark_unsupported".into()),
                );
                return;
            }
            PollOutcome::Denied => {
                set_status(
                    &session,
                    generation,
                    ScanStatus::Error("access_denied".into()),
                );
                return;
            }
            PollOutcome::Expired => {
                set_status(&session, generation, ScanStatus::Expired);
                return;
            }
            PollOutcome::Protocol { code, .. } => {
                set_status(&session, generation, ScanStatus::Error(code));
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
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

    #[test]
    fn qr_url_appends_source_with_amp_when_query_exists() {
        let u = build_qr_url("https://accounts.feishu.cn/o?x=1");
        assert!(u.starts_with("https://accounts.feishu.cn/o?x=1"), "got: {u}");
        assert!(u.contains("&source=weft"), "got: {u}");
    }

    #[test]
    fn qr_url_uses_question_mark_when_no_query() {
        let u = build_qr_url("https://accounts.feishu.cn/o");
        assert!(u.contains("?source=weft"), "got: {u}");
    }

    #[test]
    fn qr_data_uri_is_svg_base64() {
        let d = qr_svg_data_uri("https://example.com/abc").unwrap();
        assert!(
            d.starts_with("data:image/svg+xml;base64,"),
            "prefix: {}",
            &d[..d.len().min(40)]
        );
        assert!(d.len() > 100, "len: {}", d.len());
    }

    // ── device-flow service ──
    use std::collections::VecDeque;

    struct FakeTransport {
        begin: serde_json::Value,
        polls: Mutex<VecDeque<serde_json::Value>>,
    }
    #[async_trait::async_trait]
    impl RegistrationTransport for FakeTransport {
        async fn post_form(
            &self,
            _url: &str,
            form: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let action = form
                .iter()
                .find(|(k, _)| *k == "action")
                .map(|(_, v)| *v)
                .unwrap_or("");
            if action == "begin" {
                Ok(self.begin.clone())
            } else {
                Ok(self
                    .polls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop_front()
                    .unwrap_or_else(|| serde_json::json!({"error": "expired_token"})))
            }
        }
    }

    type SuccessLog = Arc<Mutex<Vec<(String, String, String)>>>;
    fn record_success() -> (OnSuccess, SuccessLog) {
        let log: SuccessLog = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let cb: OnSuccess = Arc::new(move |id: String, sec: String, open: String| {
            let l = l.clone();
            Box::pin(async move {
                l.lock().unwrap_or_else(|e| e.into_inner()).push((id, sec, open));
                Ok(())
            }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
        });
        (cb, log)
    }

    #[tokio::test]
    async fn do_begin_returns_qr_and_codes() {
        let t = FakeTransport {
            begin: serde_json::json!({
                "device_code": "dc",
                "verification_uri_complete": "https://accounts.feishu.cn/o?x=1",
                "interval": 3, "expire_in": 100
            }),
            polls: Mutex::new(VecDeque::new()),
        };
        let (view, dc, interval, expire) = do_begin(&t).await.unwrap();
        assert!(view.qr_data_uri.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(dc, "dc");
        assert_eq!(interval, 3);
        assert_eq!(expire, 100);
    }

    #[tokio::test]
    async fn poll_loop_success_invokes_callback_and_sets_status() {
        let session = Arc::new(Mutex::new(Session {
            status: ScanStatus::Pending,
            generation: 1,
        }));
        let t: Arc<dyn RegistrationTransport> = Arc::new(FakeTransport {
            begin: serde_json::json!({}),
            polls: Mutex::new(VecDeque::from(vec![serde_json::json!({
                "client_id": "cli_x", "client_secret": "sec",
                "user_info": {"open_id": "ou_1"}
            })])),
        });
        let (cb, log) = record_success();
        poll_loop(t, session.clone(), 1, "dc".into(), 0, 100, cb).await;
        assert_eq!(
            session.lock().unwrap_or_else(|e| e.into_inner()).status,
            ScanStatus::Success
        );
        assert_eq!(
            log.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            &[("cli_x".to_string(), "sec".to_string(), "ou_1".to_string())]
        );
    }

    #[tokio::test]
    async fn poll_loop_lark_sets_error_without_callback() {
        let session = Arc::new(Mutex::new(Session {
            status: ScanStatus::Pending,
            generation: 1,
        }));
        let t: Arc<dyn RegistrationTransport> = Arc::new(FakeTransport {
            begin: serde_json::json!({}),
            polls: Mutex::new(VecDeque::from(vec![
                serde_json::json!({"user_info": {"tenant_brand": "lark"}}),
            ])),
        });
        let (cb, log) = record_success();
        poll_loop(t, session.clone(), 1, "dc".into(), 0, 100, cb).await;
        assert_eq!(
            session.lock().unwrap_or_else(|e| e.into_inner()).status,
            ScanStatus::Error("lark_unsupported".into())
        );
        assert!(log.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[tokio::test]
    async fn poll_loop_denied_sets_error() {
        let session = Arc::new(Mutex::new(Session {
            status: ScanStatus::Pending,
            generation: 1,
        }));
        let t: Arc<dyn RegistrationTransport> = Arc::new(FakeTransport {
            begin: serde_json::json!({}),
            polls: Mutex::new(VecDeque::from(vec![serde_json::json!({"error": "access_denied"})])),
        });
        let (cb, _log) = record_success();
        poll_loop(t, session.clone(), 1, "dc".into(), 0, 100, cb).await;
        assert_eq!(
            session.lock().unwrap_or_else(|e| e.into_inner()).status,
            ScanStatus::Error("access_denied".into())
        );
    }
}
