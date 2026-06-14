//! 带外会话信息(M2):codex / opencode 的 model / window / MCP server 不在事件流里,
//! 各从子进程(`codex mcp list --json`)、config、模型缓存、opencode 的 ro DB 与
//! app-lifetime serve 取。MCP **tool 目录两家都不给**(已定:只列 server + 状态)。
//! claude 不走这里——它的 meta 全在 `system/init` 事件 + 引擎缓存里。

use crate::lead_chat::proto::McpServer;

/// 一次带外取数的结果。字段对齐前端 `SessionMeta`(snake_case 序列化)。
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SessionMetaSnapshot {
    pub context_tokens: Option<u64>,
    pub window: Option<u64>,
    pub model: Option<String>,
    /// None = 探测失败 / 不可用(前端保留旧 server 行);Some = 权威结果(替换,即使
    /// 为空——会话此刻确实没有 MCP server,该清掉陈旧行)。区分"瞬时失败"与"真的没有"。
    pub mcp_servers: Option<Vec<McpServer>>,
}

// ───────────────────────── 纯解析(可单测) ─────────────────────────

/// `codex mcp list --json` 输出 → server 列表;status 取自 `enabled`。
/// 返回 `Option`,区分**权威空**与**解析失败**(贯彻 [`SessionMetaSnapshot::mcp_servers`]
/// 的不变量):顶层是数组 → `Some`(可空,会话真的没 server);非 JSON / 非数组(进程
/// success 但输出畸形 / API 漂移)→ `None`,前端保留旧行,不把"没读懂"当成"没有"。
pub fn parse_codex_mcp_servers(json: &str) -> Option<Vec<McpServer>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = v.as_array()?;
    Some(
        arr.iter()
            .filter_map(|s| {
                let name = s["name"].as_str()?.to_string();
                let status = if s["enabled"].as_bool().unwrap_or(false) {
                    "connected"
                } else {
                    "disabled"
                };
                Some(McpServer {
                    name,
                    status: status.into(),
                })
            })
            .collect(),
    )
}

/// `~/.codex/models_cache.json` 内容 + 模型名 → context_window(按 `slug` 匹配)。
pub fn codex_window_from_cache(cache_json: &str, model: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(cache_json).ok()?;
    v["models"].as_array()?.iter().find_map(|m| {
        if m["slug"].as_str() == Some(model) {
            m["context_window"].as_u64()
        } else {
            None
        }
    })
}

/// codex `config.toml` 顶层 `model`。用真正的 TOML 解析(正确处理行内注释 / 引号 /
/// 转义),也不会被 `model_reasoning_effort` 等同前缀键迷惑。
pub fn parse_toml_model(cfg: &str) -> Option<String> {
    toml::from_str::<toml::Value>(cfg)
        .ok()?
        .get("model")?
        .as_str()
        .map(String::from)
}

/// codex `config.toml` 顶层 `model_context_window` 显式覆盖(整数)。优先于 models_cache。
pub fn parse_toml_context_window(cfg: &str) -> Option<u64> {
    toml::from_str::<toml::Value>(cfg)
        .ok()?
        .get("model_context_window")?
        .as_integer()
        .filter(|i| *i > 0)
        .map(|i| i as u64)
}

/// 由 opencode message 行的 `providerID`/`modelID` 拼出 `{id, providerID}` JSON,语义同
/// 旧 `session.model` 列,供 [`opencode_model_label`]/`_id`/`_provider` 复用。modelID 缺失
/// → None。**必要**:当前 opencode 的 `session.model` 列对最新会话常为空,model 身份只在
/// message 行可靠(`providerID`/`modelID`),故从 message 取并在此重组。
pub fn opencode_model_json(provider_id: Option<String>, model_id: Option<String>) -> Option<String> {
    let id = model_id.filter(|s| !s.is_empty())?;
    match provider_id.filter(|s| !s.is_empty()) {
        Some(p) => Some(serde_json::json!({ "id": id, "providerID": p }).to_string()),
        None => Some(serde_json::json!({ "id": id }).to_string()),
    }
}

/// opencode `session.model` JSON → 展示标签 `providerID/id`(无 provider 时退回 id)。
pub fn opencode_model_label(model_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(model_json).ok()?;
    let id = v["id"].as_str()?;
    Some(match v["providerID"].as_str() {
        Some(p) if !p.is_empty() => format!("{p}/{id}"),
        _ => id.to_string(),
    })
}

/// opencode `session.model` JSON → 裸 model id(用于在 /config/providers 里查 window)。
pub fn opencode_model_id(model_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(model_json)
        .ok()?["id"]
        .as_str()
        .map(String::from)
}

/// opencode `session.model` JSON → providerID(同一 model id 可能在多个 provider 下)。
pub fn opencode_model_provider(model_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(model_json)
        .ok()?["providerID"]
        .as_str()
        .map(String::from)
}

/// opencode `GET /config/providers` 响应 +(providerID, model id)→ `limit.context`。
/// 结构(实测):`{providers:[{id, models:{<id>:{limit:{context:N}}}}]}`。先按 providerID
/// 锁定 provider(同名 model id 可能跨多个 provider),拿不到 providerID 时退回首个命中。
pub fn find_model_context(
    providers: &serde_json::Value,
    provider_id: Option<&str>,
    model_id: &str,
) -> Option<u64> {
    let ps = providers["providers"].as_array()?;
    let ctx = |p: &serde_json::Value| p["models"][model_id]["limit"]["context"].as_u64();
    match provider_id {
        Some(pid) => ps.iter().find(|p| p["id"].as_str() == Some(pid)).and_then(ctx),
        None => ps.iter().find_map(ctx),
    }
}

/// opencode `GET /mcp` 响应(`{name:{status}}`)→ server 列表。返回 `Option`,同
/// [`parse_codex_mcp_servers`] 的不变量:body 是对象 → `Some`(空 `{}` = 权威无 server);
/// 非对象(2xx 但 body 畸形 / API 漂移,如返回 `[]`/`null`)→ `None`,前端保留旧行。
pub fn parse_opencode_mcp(v: &serde_json::Value) -> Option<Vec<McpServer>> {
    v.as_object().map(|o| {
        o.iter()
            .map(|(name, s)| McpServer {
                name: name.clone(),
                status: s["status"].as_str().unwrap_or("unknown").to_string(),
            })
            .collect()
    })
}

// ───────────────────────── 带外取数(I/O) ─────────────────────────

fn codex_home() -> std::path::PathBuf {
    // CODEX_HOME 优先:codex 文档把它定义为 state root,我们 spawn 的 codex 子进程与
    // `codex mcp list` 探测都从这里读 config.toml / models_cache.json。它若未设才回退
    // ~/.codex。dirs::home_dir() 平台感知(HOME 在 Windows GUI 启动常未设)。
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|s| !s.is_empty()) {
        return std::path::PathBuf::from(h);
    }
    dirs::home_dir().map(|h| h.join(".codex")).unwrap_or_default()
}

/// codex:`codex mcp list --json`(servers)+ config.toml(model)+ models_cache(window)。
/// **cwd-aware**:在 worker cwd 下跑 `codex mcp list`(拾取项目级 MCP 配置),model 也
/// 优先项目级 `<cwd>/.codex/config.toml`、回退全局 `~/.codex/config.toml`。
/// context_tokens 走 live `turn.completed`,不在这里取。
async fn gather_codex(cwd: &str) -> SessionMetaSnapshot {
    let proj_cfg = std::fs::read_to_string(std::path::Path::new(cwd).join(".codex/config.toml")).ok();
    let global_cfg = std::fs::read_to_string(codex_home().join("config.toml")).ok();
    let model = proj_cfg
        .as_deref()
        .and_then(parse_toml_model)
        .or_else(|| global_cfg.as_deref().and_then(parse_toml_model));
    // window:先认显式 `model_context_window` 覆盖(项目 → 全局),再回退 per-model 缓存。
    let window = proj_cfg
        .as_deref()
        .and_then(parse_toml_context_window)
        .or_else(|| global_cfg.as_deref().and_then(parse_toml_context_window))
        .or_else(|| {
            model.as_deref().and_then(|m| {
                std::fs::read_to_string(codex_home().join("models_cache.json"))
                    .ok()
                    .and_then(|c| codex_window_from_cache(&c, m))
            })
        });
    // `codex mcp list` 成功且输出是数组 → 权威(Some,可空);进程失败 / 输出畸形 / 超时 →
    // None(前端保留旧行,不把"没读懂"当成"没有")。10s 上限 + kill_on_drop:codex 探测
    // 若卡住(网络型 MCP 发现 / stdin 提示)不会无限挂起本次取数,与 opencode 探测的超时对齐。
    let probe = tokio::process::Command::new("codex")
        .args(["mcp", "list", "--json"])
        .current_dir(cwd)
        .kill_on_drop(true)
        .output();
    let mcp_servers = match tokio::time::timeout(std::time::Duration::from_secs(10), probe).await {
        Ok(Ok(o)) if o.status.success() => {
            parse_codex_mcp_servers(&String::from_utf8_lossy(&o.stdout))
        }
        _ => None,
    };
    SessionMetaSnapshot {
        context_tokens: None,
        window,
        model,
        mcp_servers,
    }
}

/// opencode:ro DB(usage + model)+ app-lifetime serve(window + servers)。
async fn gather_opencode(cwd: &str, native_id: Option<&str>) -> SessionMetaSnapshot {
    let (ctx, model_json) = match native_id {
        Some(id) => crate::sidecar::opencode_session_usage(id)
            .await
            .unwrap_or((0, None)),
        None => (0, None),
    };
    let model = model_json.as_deref().and_then(opencode_model_label);
    let model_id = model_json.as_deref().and_then(opencode_model_id);
    let provider_id = model_json.as_deref().and_then(opencode_model_provider);
    let (window, mcp_servers) = crate::opencode::server_window_and_mcp(
        cwd,
        provider_id.as_deref(),
        model_id.as_deref(),
    )
    .await;
    SessionMetaSnapshot {
        context_tokens: (ctx > 0).then_some(ctx),
        window,
        model,
        mcp_servers,
    }
}

/// 按 tool 分派。claude 不走这里(返回空)。
pub async fn gather(tool: &str, cwd: &str, native_id: Option<&str>) -> SessionMetaSnapshot {
    match tool {
        "codex" => gather_codex(cwd).await,
        "opencode" => gather_opencode(cwd, native_id).await,
        _ => SessionMetaSnapshot::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_mcp_servers_status_from_enabled() {
        let j = r#"[{"name":"codegraph","enabled":true,"auth_status":"unsupported"},{"name":"old","enabled":false}]"#;
        let s = parse_codex_mcp_servers(j).expect("array parses to Some");
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            McpServer {
                name: "codegraph".into(),
                status: "connected".into()
            }
        );
        assert_eq!(s[1].status, "disabled");
        // 权威空(会话真的没 server)vs 解析失败 —— 必须可区分:
        assert_eq!(parse_codex_mcp_servers("[]"), Some(vec![])); // 空数组 = 权威空
        assert_eq!(parse_codex_mcp_servers("not json"), None); // 非 JSON = 没读懂
        assert_eq!(parse_codex_mcp_servers("{}"), None); // 非数组(API 漂移)= 没读懂
    }

    #[test]
    fn codex_window_matches_slug() {
        let j = r#"{"models":[{"slug":"gpt-5.5","context_window":272000},{"slug":"o9","context_window":400000}]}"#;
        assert_eq!(codex_window_from_cache(j, "gpt-5.5"), Some(272000));
        assert_eq!(codex_window_from_cache(j, "nope"), None);
    }

    #[test]
    fn toml_model_picks_bare_model_line() {
        let t = "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"xhigh\"\n";
        assert_eq!(parse_toml_model(t), Some("gpt-5.5".into()));
        // 行内注释不能漏进 value(回归 Codex review)。
        assert_eq!(
            parse_toml_model("model = \"gpt-5.5\"  # work profile\n"),
            Some("gpt-5.5".into())
        );
        // 不是合法 TOML → None,不 panic。
        assert_eq!(parse_toml_model("model = "), None);
    }

    #[test]
    fn toml_model_not_confused_by_reasoning_effort() {
        assert_eq!(parse_toml_model("model_reasoning_effort = \"xhigh\"\n"), None);
    }

    #[test]
    fn toml_context_window_override_beats_cache() {
        // 显式 `model_context_window` 是面板分母的权威覆盖,优先于 per-model 缓存。
        assert_eq!(
            parse_toml_context_window("model = \"gpt-5.5\"\nmodel_context_window = 400000\n"),
            Some(400_000)
        );
        // 无覆盖键 → None(回退缓存)。
        assert_eq!(parse_toml_context_window("model = \"gpt-5.5\"\n"), None);
        // 非正数 / 非整数都不算覆盖。
        assert_eq!(parse_toml_context_window("model_context_window = 0\n"), None);
        assert_eq!(
            parse_toml_context_window("model_context_window = \"big\"\n"),
            None
        );
    }

    #[test]
    fn opencode_model_json_from_message_fields() {
        // message 行的 providerID/modelID → {id,providerID} JSON,喂回三个解析器。
        let j = opencode_model_json(Some("kimi-for-coding".into()), Some("k2p6".into()))
            .expect("modelID present → Some");
        assert_eq!(opencode_model_label(&j).as_deref(), Some("kimi-for-coding/k2p6"));
        assert_eq!(opencode_model_id(&j).as_deref(), Some("k2p6"));
        assert_eq!(opencode_model_provider(&j).as_deref(), Some("kimi-for-coding"));
        // provider 缺失 → 只 id,label 退回裸 id。
        let j2 = opencode_model_json(None, Some("k2p6".into())).expect("modelID present → Some");
        assert_eq!(opencode_model_label(&j2).as_deref(), Some("k2p6"));
        assert_eq!(opencode_model_provider(&j2), None);
        // modelID 缺失 / 空 → None(没有可展示的 model)。
        assert_eq!(opencode_model_json(Some("p".into()), None), None);
        assert_eq!(opencode_model_json(Some("p".into()), Some(String::new())), None);
    }

    #[test]
    fn opencode_model_label_id_provider() {
        let m = r#"{"id":"k2p6","providerID":"kimi-for-coding","variant":"default"}"#;
        assert_eq!(opencode_model_label(m).as_deref(), Some("kimi-for-coding/k2p6"));
        assert_eq!(opencode_model_id(m).as_deref(), Some("k2p6"));
        assert_eq!(opencode_model_provider(m).as_deref(), Some("kimi-for-coding"));
        assert_eq!(opencode_model_label(r#"{"id":"x"}"#).as_deref(), Some("x"));
        assert_eq!(opencode_model_provider(r#"{"id":"x"}"#), None);
    }

    #[test]
    fn finds_model_context_by_provider_and_id() {
        // 同名 model id 跨两个 provider、window 不同 —— 必须按 providerID 锁定。
        let v: serde_json::Value = serde_json::from_str(
            r#"{"providers":[
                {"id":"kimi","models":{"k2":{"limit":{"context":262144}}}},
                {"id":"acme","models":{"k2":{"limit":{"context":99}}}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(find_model_context(&v, Some("kimi"), "k2"), Some(262144));
        assert_eq!(find_model_context(&v, Some("acme"), "k2"), Some(99));
        assert_eq!(find_model_context(&v, None, "k2"), Some(262144)); // 首个命中
        assert_eq!(find_model_context(&v, Some("kimi"), "missing"), None);
        assert_eq!(find_model_context(&v, Some("nope"), "k2"), None);
    }

    #[test]
    fn parses_opencode_mcp_map() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"codegraph":{"status":"connected"},"x":{"status":"failed"}}"#)
                .unwrap();
        let s = parse_opencode_mcp(&v).expect("object parses to Some");
        assert_eq!(s.len(), 2);
        assert!(s.iter().any(|m| m.name == "codegraph" && m.status == "connected"));
        assert!(s.iter().any(|m| m.name == "x" && m.status == "failed"));
        // 权威空 vs body 畸形(2xx 但非对象):
        assert_eq!(parse_opencode_mcp(&serde_json::json!({})), Some(vec![])); // {} = 权威空
        assert_eq!(parse_opencode_mcp(&serde_json::json!([])), None); // 非对象 = 没读懂
        assert_eq!(parse_opencode_mcp(&serde_json::json!(null)), None);
    }
}
