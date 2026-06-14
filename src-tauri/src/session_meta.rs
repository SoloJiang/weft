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
    pub mcp_servers: Vec<McpServer>,
}

// ───────────────────────── 纯解析(可单测) ─────────────────────────

/// `codex mcp list --json` 输出 → server 列表;status 取自 `enabled`。
pub fn parse_codex_mcp_servers(json: &str) -> Vec<McpServer> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
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
        .collect()
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

/// 从 `config.toml` 抠裸 `model = "..."`(不被 `model_reasoning_effort` 等迷惑)。
pub fn parse_toml_model(toml: &str) -> Option<String> {
    toml.lines()
        .map(str::trim)
        .find(|l| l.starts_with("model ") || l.starts_with("model="))
        .and_then(|l| {
            let rest = l.split_once('=')?.1.trim();
            Some(rest.trim_matches('"').to_string()).filter(|s| !s.is_empty())
        })
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

/// opencode `GET /mcp` 响应(`{name:{status}}`)→ server 列表。
pub fn parse_opencode_mcp(v: &serde_json::Value) -> Vec<McpServer> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(name, s)| McpServer {
                    name: name.clone(),
                    status: s["status"].as_str().unwrap_or("unknown").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ───────────────────────── 带外取数(I/O) ─────────────────────────

fn codex_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".codex"))
        .unwrap_or_default()
}

/// codex:`codex mcp list --json`(servers)+ config.toml(model)+ models_cache(window)。
/// **cwd-aware**:在 worker cwd 下跑 `codex mcp list`(拾取项目级 MCP 配置),model 也
/// 优先项目级 `<cwd>/.codex/config.toml`、回退全局 `~/.codex/config.toml`。
/// context_tokens 走 live `turn.completed`,不在这里取。
async fn gather_codex(cwd: &str) -> SessionMetaSnapshot {
    let model = std::fs::read_to_string(std::path::Path::new(cwd).join(".codex/config.toml"))
        .ok()
        .and_then(|t| parse_toml_model(&t))
        .or_else(|| {
            std::fs::read_to_string(codex_home().join("config.toml"))
                .ok()
                .and_then(|t| parse_toml_model(&t))
        });
    let window = model.as_deref().and_then(|m| {
        std::fs::read_to_string(codex_home().join("models_cache.json"))
            .ok()
            .and_then(|c| codex_window_from_cache(&c, m))
    });
    let mcp_servers = tokio::process::Command::new("codex")
        .args(["mcp", "list", "--json"])
        .current_dir(cwd)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_codex_mcp_servers(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
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
        let s = parse_codex_mcp_servers(j);
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            McpServer {
                name: "codegraph".into(),
                status: "connected".into()
            }
        );
        assert_eq!(s[1].status, "disabled");
        assert!(parse_codex_mcp_servers("not json").is_empty());
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
    }

    #[test]
    fn toml_model_not_confused_by_reasoning_effort() {
        assert_eq!(parse_toml_model("model_reasoning_effort = \"xhigh\"\n"), None);
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
        let s = parse_opencode_mcp(&v);
        assert_eq!(s.len(), 2);
        assert!(s.iter().any(|m| m.name == "codegraph" && m.status == "connected"));
        assert!(s.iter().any(|m| m.name == "x" && m.status == "failed"));
    }
}
