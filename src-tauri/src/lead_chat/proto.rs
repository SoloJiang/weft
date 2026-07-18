//! Parse one stdout line of a headless agent CLI into the few shapes the chat
//! engine cares about. Three dialects, all spike-verified live:
//! - claude: `-p --output-format stream-json` (long-lived, deltas + result)
//! - codex: `exec --json` (per-turn; thread.started / item.* / turn.completed)
//! - opencode: `run --format json` (per-turn; text / tool_use, EOF ends turn)
//! Unknown lines are Other and ignored.

use serde_json::Value;

/// One slash command for the composer palette: the token plus whatever metadata
/// the CLI reported. `name` is the match + dispatch key; claude's `initialize`
/// adds `description` + `argumentHint`, opencode's GET /command adds a description.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SlashCmd {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg_hint: Option<String>,
}

impl SlashCmd {
    /// Name-only command (claude's init list, codex prompt stems): no metadata.
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            arg_hint: None,
        }
    }
}

/// 一个 MCP server 的连接态,来自 claude `system/init.mcp_servers`。
/// Deserialize:引擎 meta 快照(`PersistedMeta`)落库后要在重启时读回。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpServer {
    pub name: String,
    pub status: String, // connected | pending | failed | …(原样透传)
}

/// A tool invocation captured from the stream. Carries enough to render an
/// expandable row — the full `input` plus a compact `summary` for the collapsed
/// line. `output` is set only when the dialect delivers the result inline
/// (opencode's completed `tool_use`); claude and codex send results separately as
/// `ToolResults`, merged into the running row by `id`.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub summary: String,
    pub output: Option<String>,
    pub is_error: bool,
}

/// One tool result block (claude `user` message), correlated to its `Assistant`
/// tool call by `id` (= the call's `tool_use_id`).
#[derive(Debug, Clone)]
pub struct ToolResultItem {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug)]
pub enum ChatEvent {
    Init {
        session_id: String,
        slash_commands: Vec<SlashCmd>,
        /// claude `system/init` 才有;codex/opencode 走 Commands/带外路径,留空。
        mcp_servers: Vec<McpServer>,
        tools: Vec<String>,
        model: Option<String>,
    },
    TextDelta {
        text: String,
    },
    /// One complete assistant message event: its text blocks plus any tool calls
    /// it started. Codex builds pill-only calls (transient activity); claude and
    /// opencode build full calls persisted as expandable tool rows. `uuid` is the
    /// claude transcript uuid (rewind anchor); other dialects leave it None.
    Assistant {
        texts: Vec<String>,
        tools: Vec<ToolCall>,
        uuid: Option<String>,
    },
    /// Tool results delivered out-of-band (claude `user` message), each
    /// correlated to its `Assistant` tool call by id.
    ToolResults {
        items: Vec<ToolResultItem>,
    },
    TurnEnd {
        is_error: bool,
        /// 当前上下文 token(input + cache_read + cache_creation);拿不到为 None。
        context_tokens: Option<u64>,
    },
    /// 带外 token 用量(codex app-server `thread/tokenUsage/updated`):当前上下文
    /// (最近一次模型调用的 input)+ 模型窗口。区别于 TurnEnd 的会话级累计。
    Usage {
        context_tokens: u64,
        window: Option<u64>,
    },
    /// Response to our `initialize` control_request: the CLI's slash commands.
    /// Sent right after spawn — the `init` system message only arrives with the
    /// FIRST user turn, far too late for the composer's palette.
    Commands {
        commands: Vec<SlashCmd>,
    },
    Other,
}

/// Dialect dispatch: per-tool line parser.
pub fn parse_line_for(tool: &str, line: &str) -> ChatEvent {
    match tool {
        "codex" => parse_codex(line),
        "opencode" => parse_opencode(line),
        _ => parse_line(line),
    }
}

/// Best-effort native session id from a line (per-turn dialects carry it on
/// their events; claude's comes via the init/system path instead).
pub fn extract_native(tool: &str, line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    match tool {
        "codex" => v
            .get("thread_id")
            .and_then(|t| t.as_str())
            .map(String::from),
        "opencode" => v
            .get("sessionID")
            .and_then(|s| s.as_str())
            .map(String::from),
        _ => None,
    }
}

fn parse_codex(line: &str) -> ChatEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return ChatEvent::Other;
    };
    let kind = v["type"].as_str();
    match kind {
        Some("item.started") | Some("item.completed") => {
            let completed = kind == Some("item.completed");
            let item = &v["item"];
            match item["type"].as_str() {
                // codex exec has no deltas: full text arrives on completed.
                Some("agent_message") if completed => ChatEvent::Assistant {
                    texts: item["text"]
                        .as_str()
                        .map(|t| vec![t.to_string()])
                        .unwrap_or_default(),
                    tools: vec![],
                    uuid: None,
                },
                Some("agent_message") => ChatEvent::Other,
                Some("error") => ChatEvent::TextDelta {
                    text: error_text_from_item(item),
                },
                Some("reasoning") => ChatEvent::Other,
                // Real tool items → rows (started: running; completed: result, merged
                // by item id). Tool-call types: exec/edit/MCP + subagent + dynamic
                // (snake_case of the 0.139.0 ThreadItem union). Content items (plan,
                // reasoning, …) are ignored so they don't render as empty tool rows.
                Some(
                    "command_execution" | "file_change" | "mcp_tool_call"
                    | "collab_agent_tool_call" | "dynamic_tool_call",
                ) => {
                    if completed {
                        ChatEvent::ToolResults {
                            items: vec![codex_tool_result(item)],
                        }
                    } else {
                        ChatEvent::Assistant {
                            texts: vec![],
                            tools: vec![codex_tool_call(item)],
                            uuid: None,
                        }
                    }
                }
                // Other content items (/plan, /review …) carry their text only on
                // completion (exec has no deltas) — surface it so it isn't dropped;
                // the running half and payload-less items stay Other (no empty row).
                _ if completed => match codex_content_item_text(item) {
                    Some(text) => ChatEvent::Assistant { texts: vec![text], tools: vec![], uuid: None },
                    None => ChatEvent::Other,
                },
                _ => ChatEvent::Other,
            }
        }
        Some("turn.completed") => ChatEvent::TurnEnd {
            is_error: false,
            // exec 的 input_tokens 是会话累计吞吐量,不是当前上下文(实测会随 resume 累加),
            // 当 % 分子会涨到 100% —— 不上报;准确值由 app-server 的 last_token_usage 给。
            context_tokens: None,
        },
        Some("turn.failed") | Some("error") => ChatEvent::TurnEnd {
            is_error: true,
            context_tokens: None,
        },
        _ => ChatEvent::Other,
    }
}

/// Display text of a non-tool content item (/plan, /review …): the first
/// non-empty string field it carries, or None when there's nothing to show.
/// Shared by the exec and app-server dialects (same field names).
pub(crate) fn codex_content_item_text(item: &Value) -> Option<String> {
    ["text", "review", "plan", "message"].iter().find_map(|k| {
        let s = item[*k].as_str()?.trim();
        (!s.is_empty()).then(|| s.to_string())
    })
}

/// Running `ToolCall` from a codex `item.started` tool item; `output` filled
/// later by the matching `item.completed`.
fn codex_tool_call(item: &Value) -> ToolCall {
    ToolCall {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        name: item["type"].as_str().unwrap_or("tool").to_string(),
        input: cap_input(codex_tool_input(item)),
        summary: codex_tool_summary(item),
        output: None,
        is_error: false,
    }
}

/// Result of a codex `item.completed` tool item, keyed by item id so the engine
/// merges its output into the running row.
fn codex_tool_result(item: &Value) -> ToolResultItem {
    ToolResultItem {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        output: cap_output(codex_item_output(item)),
        is_error: codex_item_is_error(item),
    }
}

/// The call's descriptive fields (command/cwd, changes/path, mcp server/tool/args)
/// for the row's Input block; skips status/output/id noise.
fn codex_tool_input(item: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for k in [
        "command",
        "cwd",
        "changes",
        "path",
        "server",
        "tool",
        "arguments",
        "invocation",
    ] {
        match item.get(k) {
            Some(val) if !val.is_null() => {
                obj.insert(k.to_string(), val.clone());
            }
            _ => {}
        }
    }
    Value::Object(obj)
}

/// A compact, truncated summary for the collapsed tool row.
fn codex_tool_summary(item: &Value) -> String {
    let s = ["command", "tool", "path", "name"]
        .iter()
        .find_map(|k| item[k].as_str())
        .map(String::from)
        .or_else(|| item["changes"][0]["path"].as_str().map(String::from))
        .unwrap_or_default();
    s.chars().take(120).collect()
}

/// Output text for the expanded row: command stdout / mcp result, or — for a
/// file_change (no inline diff) — a `kind  path` line per change.
fn codex_item_output(item: &Value) -> String {
    // command stdout / inline diff are plain strings.
    if let Some(s) = ["aggregated_output", "unified_diff", "diff"]
        .iter()
        .find_map(|k| item[k].as_str())
    {
        return s.to_string();
    }
    // mcp_tool_call output/result: a string, an MCP result object
    // (`{content:[{text}]}` — what weft's bus/planner tools return), or another
    // JSON value. Render the text where possible, else serialize so the row
    // isn't blank when the tool actually returned data.
    for key in ["output", "result", "error"] {
        let v = &item[key];
        if v.is_null() {
            continue;
        }
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        if let Some(content) = v["content"].as_array() {
            let text = content
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
        return v.to_string();
    }
    if let Some(changes) = item["changes"].as_array() {
        return changes
            .iter()
            .filter_map(|c| {
                let path = c["path"].as_str()?;
                let kind = c["kind"].as_str().unwrap_or("change");
                Some(format!("{kind}  {path}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// Whether a completed codex tool item failed: a non-zero `exit_code`
/// (command_execution) or a failed/error `status`.
fn codex_item_is_error(item: &Value) -> bool {
    // A declined/canceled approval completes the item without running it — not a
    // success. Check status first; otherwise a non-zero exit code is an error.
    if matches!(
        item["status"].as_str(),
        Some("failed" | "error" | "declined" | "canceled" | "cancelled")
    ) {
        return true;
    }
    item["exit_code"].as_i64().is_some_and(|c| c != 0)
}

fn parse_opencode(line: &str) -> ChatEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return ChatEvent::Other;
    };
    let part = &v["part"];
    match v["type"].as_str() {
        Some("text") => ChatEvent::Assistant {
            texts: part["text"]
                .as_str()
                .map(|t| vec![t.to_string()])
                .unwrap_or_default(),
            tools: vec![],
            uuid: None,
        },
        Some("tool_use") => {
            let state = &part["state"];
            let status = state["status"].as_str().unwrap_or("");
            // opencode re-emits this part as the tool progresses and gives no
            // stable per-call id to dedupe running→completed frames, so surface a
            // single row only once it has finished, where input AND output are
            // both present.
            if status != "completed" && status != "error" {
                return ChatEvent::Other;
            }
            let input = state["input"].clone();
            let summary = compact_input(&input);
            ChatEvent::Assistant {
                texts: vec![],
                tools: vec![ToolCall {
                    id: String::new(),
                    name: part["tool"].as_str().unwrap_or("tool").to_string(),
                    input: cap_input(input),
                    summary,
                    output: Some(opencode_output(state)),
                    is_error: status == "error",
                }],
                uuid: None,
            }
        }
        _ => ChatEvent::Other,
    }
}

/// Best-effort result text from an opencode completed tool `state`. The field
/// name varies by tool; try the common ones and fall back to empty (the row
/// still shows the input).
fn opencode_output(state: &Value) -> String {
    let text = ["output", "result", "stdout", "metadata"]
        .iter()
        .find_map(|k| state[k].as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();
    cap_output(text)
}

/// Cap tool output so a pathological stdout / large file read can't bloat the
/// persisted row, its push payload, or the React store. The collapsed row shows
/// only a summary anyway, and the expanded view already paginates.
fn cap_output(s: String) -> String {
    const MAX: usize = 16_000;
    if s.chars().count() <= MAX {
        return s;
    }
    let mut out: String = s.chars().take(MAX).collect();
    out.push_str("\n… (truncated)");
    out
}

/// Cap a tool input the same way: a huge payload (e.g. a claude `Write`/`Edit`
/// carrying full file contents) is replaced by a truncated string so it can't
/// bloat the persisted row, its push, or the store. Small inputs pass through
/// unchanged so the UI still renders the structured object.
pub(crate) fn cap_input(input: Value) -> Value {
    const MAX: usize = 16_000;
    let s = input.to_string();
    if s.chars().count() <= MAX {
        return input;
    }
    let mut capped: String = s.chars().take(MAX).collect();
    capped.push_str("… (truncated)");
    Value::String(capped)
}

pub(crate) fn error_text_from_item(item: &Value) -> String {
    let text = item["message"]
        .as_str()
        .or_else(|| item["text"].as_str())
        .or_else(|| item["summary"].as_str())
        .or_else(|| item["detail"].as_str())
        .or_else(|| item["error"]["message"].as_str())
        .or_else(|| item["error"].as_str())
        .unwrap_or("Codex reported an error.");
    humanize_error_text(text)
}

/// Dig the human message out of an error payload that may itself BE raw JSON:
/// codex passes the provider's API error response straight through as the item
/// message (`{"type":"error","status":400,"error":{"message":…}}`) — sometimes
/// several objects CONCATENATED after stream retries — which used to render
/// verbatim in chat. Plain text passes through unchanged; so does JSON we
/// can't find a message in (better raw than blank).
pub(crate) fn humanize_error_text(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    // First object of a possibly-concatenated stream ("{…}{…}").
    let mut objects = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    if let Some(Ok(v)) = objects.next() {
        let msg = v["error"]["message"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .or_else(|| v["error"].as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty());
        if let Some(m) = msg {
            return m.to_string();
        }
    }
    trimmed.to_string()
}

pub fn parse_line(line: &str) -> ChatEvent {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return ChatEvent::Other;
    };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => ChatEvent::Init {
            session_id: v["session_id"].as_str().unwrap_or_default().to_string(),
            slash_commands: v["slash_commands"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(SlashCmd::bare))
                        .collect()
                })
                .unwrap_or_default(),
            mcp_servers: v["mcp_servers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| {
                            Some(McpServer {
                                name: s["name"].as_str()?.to_string(),
                                status: s["status"].as_str().unwrap_or("unknown").to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            tools: v["tools"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            model: v["model"].as_str().map(String::from),
        },
        Some("stream_event") => {
            let d = &v["event"]["delta"];
            if v["event"]["type"] == "content_block_delta" && d["type"] == "text_delta" {
                ChatEvent::TextDelta {
                    text: d["text"].as_str().unwrap_or_default().to_string(),
                }
            } else {
                ChatEvent::Other
            }
        }
        Some("assistant") => {
            let mut texts = vec![];
            let mut tools = vec![];
            for b in v["message"]["content"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                match b["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = b["text"].as_str() {
                            if !t.is_empty() {
                                texts.push(t.to_string());
                            }
                        }
                    }
                    Some("tool_use") => {
                        let input = b["input"].clone();
                        let summary = compact_input(&input);
                        tools.push(ToolCall {
                            id: b["id"].as_str().unwrap_or_default().to_string(),
                            name: b["name"].as_str().unwrap_or("tool").to_string(),
                            input: cap_input(input),
                            summary,
                            output: None,
                            is_error: false,
                        });
                    }
                    _ => {}
                }
            }
            ChatEvent::Assistant {
                texts,
                tools,
                uuid: v["uuid"].as_str().map(str::to_string),
            }
        }
        // Tool results come back as a `user` turn whose content is one or more
        // `tool_result` blocks, each tied to its call by `tool_use_id`.
        Some("user") => {
            let mut items = vec![];
            for b in v["message"]["content"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                if b["type"] == "tool_result" {
                    items.push(ToolResultItem {
                        id: b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                        output: tool_result_text(&b["content"]),
                        is_error: b["is_error"].as_bool().unwrap_or(false),
                    });
                }
            }
            if items.is_empty() {
                ChatEvent::Other
            } else {
                ChatEvent::ToolResults { items }
            }
        }
        Some("result") => ChatEvent::TurnEnd {
            is_error: v["subtype"] != "success",
            context_tokens: claude_context_tokens(&v["usage"]),
        },
        Some("control_response") => {
            let r = &v["response"];
            if r["subtype"] == "success" {
                if let Some(cmds) = r["response"]["commands"].as_array() {
                    return ChatEvent::Commands {
                        commands: cmds
                            .iter()
                            .filter_map(|c| {
                                let name = c["name"].as_str()?.to_string();
                                let pick = |k: &str| {
                                    c[k].as_str().filter(|s| !s.is_empty()).map(String::from)
                                };
                                Some(SlashCmd {
                                    name,
                                    description: pick("description"),
                                    arg_hint: pick("argumentHint"),
                                })
                            })
                            .collect(),
                    };
                }
            }
            ChatEvent::Other
        }
        _ => ChatEvent::Other,
    }
}

/// claude `result.usage` → 当前上下文占用(本回合送入的 prompt 体量)。
/// 取 input + cache_read + cache_creation;usage 缺失/非对象返回 None。
fn claude_context_tokens(usage: &Value) -> Option<u64> {
    if !usage.is_object() {
        return None;
    }
    let g = |k: &str| usage[k].as_u64().unwrap_or(0);
    Some(g("input_tokens") + g("cache_read_input_tokens") + g("cache_creation_input_tokens"))
}

/// First string-ish field of a tool input, truncated — just enough for a
/// compact activity pill. An empty/opaque input yields "" (render nothing),
/// never "{}" noise.
fn compact_input(input: &Value) -> String {
    let s = [
        "file_path",
        "path",
        "command",
        "pattern",
        "query",
        "url",
        "description",
    ]
    .iter()
    .find_map(|k| input[k].as_str())
    .map(String::from)
    .unwrap_or_else(|| match input {
        Value::Object(o) if o.is_empty() => String::new(),
        Value::Null => String::new(),
        other => other.to_string(),
    });
    s.chars().take(120).collect()
}

/// Flatten a claude `tool_result` block's `content` to text. It is either a
/// plain string or an array of content blocks (we keep the text blocks).
fn tool_result_text(content: &Value) -> String {
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        arr.iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    cap_output(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// codex passes the provider's raw API error JSON through as the item
    /// message — sometimes several objects concatenated after retries. The
    /// human message is dug out; plain text and unrecognizable JSON pass
    /// through unchanged.
    #[test]
    fn humanize_error_digs_message_out_of_raw_provider_json() {
        // The real payload observed in the smoke session (doubled by a retry).
        let raw = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'gpt-5.6-sol' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again."}}{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'gpt-5.6-sol' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again."}}"#;
        assert_eq!(
            humanize_error_text(raw),
            "The 'gpt-5.6-sol' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again."
        );
        // Flat {"message": …} shape.
        assert_eq!(humanize_error_text(r#"{"message":"rate limited"}"#), "rate limited");
        // String-typed error field.
        assert_eq!(humanize_error_text(r#"{"error":"boom"}"#), "boom");
        // Plain text passes through.
        assert_eq!(humanize_error_text("  context window exceeded  "), "context window exceeded");
        // JSON without a recognizable message stays raw (better raw than blank).
        assert_eq!(humanize_error_text(r#"{"code":500}"#), r#"{"code":500}"#);
        // Malformed JSON stays raw.
        assert_eq!(humanize_error_text("{oops"), "{oops");
        // error_text_from_item routes through the same digger.
        let item = serde_json::json!({"type":"error","message":"{\"error\":{\"message\":\"nested\"}}"});
        assert_eq!(error_text_from_item(&item), "nested");
    }

    #[test]
    fn parses_init() {
        let l = r#"{"type":"system","subtype":"init","session_id":"abc-123","slash_commands":["compact","commit"]}"#;
        match parse_line(l) {
            ChatEvent::Init {
                session_id,
                slash_commands,
                ..
            } => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(
                    slash_commands,
                    vec![SlashCmd::bare("compact"), SlashCmd::bare("commit")]
                );
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_init_mcp_tools_model() {
        // 真实形状取自本机 `claude -p --output-format stream-json` 实测。
        let l = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8[1m]","slash_commands":["compact"],"tools":["Bash","mcp__codegraph__codegraph_search"],"mcp_servers":[{"name":"codegraph","status":"connected"},{"name":"tauri","status":"pending"}]}"#;
        match parse_line(l) {
            ChatEvent::Init {
                mcp_servers,
                tools,
                model,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("claude-opus-4-8[1m]"));
                assert_eq!(tools, vec!["Bash", "mcp__codegraph__codegraph_search"]);
                assert_eq!(
                    mcp_servers,
                    vec![
                        McpServer { name: "codegraph".into(), status: "connected".into() },
                        McpServer { name: "tauri".into(), status: "pending".into() },
                    ]
                );
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_result_usage_context_tokens() {
        let l = r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":8684,"cache_creation_input_tokens":22127,"cache_read_input_tokens":0,"output_tokens":4}}"#;
        match parse_line(l) {
            ChatEvent::TurnEnd { is_error, context_tokens } => {
                assert!(!is_error);
                assert_eq!(context_tokens, Some(8684 + 22127));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn result_without_usage_has_none_tokens() {
        let l = r#"{"type":"result","subtype":"success","is_error":false}"#;
        assert!(matches!(
            parse_line(l),
            ChatEvent::TurnEnd { context_tokens: None, .. }
        ));
    }

    #[test]
    fn parses_text_delta() {
        let l = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"He"}}}"#;
        assert!(matches!(parse_line(l), ChatEvent::TextDelta { text } if text == "He"));
    }

    #[test]
    fn ignores_signature_and_thinking_deltas() {
        let l = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"xx"}}}"#;
        assert!(matches!(parse_line(l), ChatEvent::Other));
        let l2 = r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"thinking"}}}"#;
        assert!(matches!(parse_line(l2), ChatEvent::Other));
    }

    #[test]
    fn empty_tool_input_yields_blank_summary() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__weft_planner__get_task","input":{}}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0].name, "mcp__weft_planner__get_task");
                assert_eq!(tools[0].summary, "");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_assistant_blocks() {
        let l = r#"{"type":"assistant","uuid":"u-1","message":{"content":[{"type":"text","text":"done"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { texts, tools, uuid } => {
                assert_eq!(texts, vec!["done"]);
                assert_eq!(tools[0].id, "toolu_1");
                assert_eq!(tools[0].name, "Read");
                assert!(tools[0].summary.contains("b.rs"));
                // full input is kept for the expandable row, not just the summary
                assert_eq!(tools[0].input["file_path"], "/a/b.rs");
                assert!(tools[0].output.is_none());
                // transcript uuid rides along as the rewind anchor
                assert_eq!(uuid.as_deref(), Some("u-1"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_claude_tool_result() {
        // string content
        let l = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"hello\nworld","is_error":false}]}}"#;
        match parse_line(l) {
            ChatEvent::ToolResults { items } => {
                assert_eq!(items[0].id, "toolu_1");
                assert_eq!(items[0].output, "hello\nworld");
                assert!(!items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // array-of-blocks content + error flag
        let l2 = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":[{"type":"text","text":"boom"}],"is_error":true}]}}"#;
        match parse_line(l2) {
            ChatEvent::ToolResults { items } => {
                assert_eq!(items[0].id, "toolu_2");
                assert_eq!(items[0].output, "boom");
                assert!(items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // a plain user text turn (no tool_result) is not a ToolResults event
        let l3 = r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert!(matches!(parse_line(l3), ChatEvent::Other));
    }

    #[test]
    fn caps_huge_tool_output() {
        let big = "x".repeat(20_000);
        let line = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t","content":"{big}"}}]}}}}"#
        );
        match parse_line(&line) {
            ChatEvent::ToolResults { items } => {
                assert!(items[0].output.chars().count() < 20_000);
                assert!(items[0].output.ends_with("(truncated)"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn caps_huge_tool_input() {
        let big = "x".repeat(20_000);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t","name":"Write","input":{{"content":"{big}"}}}}]}}}}"#
        );
        match parse_line(&line) {
            ChatEvent::Assistant { tools, .. } => {
                // a huge object input collapses to a single truncated string
                let s = tools[0].input.as_str().expect("capped input is a string");
                assert!(s.chars().count() < 20_000);
                assert!(s.ends_with("(truncated)"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn thinking_only_assistant_is_empty() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"...","signature":"s"}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { texts, tools, .. } => {
                assert!(texts.is_empty());
                assert!(tools.is_empty());
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_codex_dialect() {
        assert_eq!(
            extract_native("codex", r#"{"type":"thread.started","thread_id":"abc-1"}"#).as_deref(),
            Some("abc-1")
        );
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"ok"}}"#,
        ) {
            ChatEvent::Assistant { texts, .. } => assert_eq!(texts, vec!["ok"]),
            e => panic!("{e:?}"),
        }
        // item.started for a tool item → a running row with id + input (shape
        // verified live, codex-cli 0.139.0).
        match parse_line_for(
            "codex",
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'echo hi'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        ) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0].name, "command_execution");
                assert_eq!(tools[0].id, "item_1"); // stable id correlates started→completed
                assert_eq!(tools[0].summary, "/bin/zsh -lc 'echo hi'");
                assert_eq!(tools[0].input["command"], "/bin/zsh -lc 'echo hi'");
                assert!(tools[0].output.is_none()); // running: filled by item.completed
            }
            e => panic!("{e:?}"),
        }
        match parse_line_for(
            "codex",
            r#"{"type":"item.started","item":{"type":"error","message":"unknown slash command"}}"#,
        ) {
            ChatEvent::TextDelta { text } => assert_eq!(text, "unknown slash command"),
            e => panic!("{e:?}"),
        }
        assert!(matches!(
            parse_line_for("codex", r#"{"type":"turn.completed","usage":{}}"#),
            ChatEvent::TurnEnd { is_error: false, context_tokens: None, .. }
        ));
    }

    #[test]
    fn codex_tool_item_completes_into_tool_result() {
        // command_execution completed → ToolResults (stdout + exit_code), keyed by
        // item id (shape verified live, codex-cli 0.139.0).
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'echo hi'","aggregated_output":"hi\n","exit_code":0,"status":"completed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => {
                assert_eq!(items[0].id, "item_1");
                assert_eq!(items[0].output, "hi\n");
                assert!(!items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // A non-zero exit code marks the result as an error.
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"false","aggregated_output":"","exit_code":1,"status":"completed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => {
                assert_eq!(items[0].id, "item_2");
                assert!(items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // agent_message still completes as assistant text, not a tool result.
        assert!(matches!(
            parse_line_for(
                "codex",
                r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"done"}}"#,
            ),
            ChatEvent::Assistant { .. }
        ));
    }

    #[test]
    fn codex_file_change_item_captures_paths() {
        // file_change has no diff, only `changes:[{path,kind}]`; output lists
        // `kind  path` (shape verified live, codex-cli 0.139.0).
        match parse_line_for(
            "codex",
            r#"{"type":"item.started","item":{"id":"item_3","type":"file_change","changes":[{"path":"/repo/hello.txt","kind":"add"}],"status":"in_progress"}}"#,
        ) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0].name, "file_change");
                assert_eq!(tools[0].id, "item_3");
                assert_eq!(tools[0].summary, "/repo/hello.txt");
                assert!(tools[0].output.is_none());
            }
            e => panic!("{e:?}"),
        }
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/repo/hello.txt","kind":"add"}],"status":"completed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => {
                assert_eq!(items[0].id, "item_3");
                assert_eq!(items[0].output, "add  /repo/hello.txt");
                assert!(!items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // A failed file_change (status) is flagged as an error.
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"item_4","type":"file_change","changes":[{"path":"/repo/x","kind":"add"}],"status":"failed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => assert!(items[0].is_error),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn codex_declined_command_completes_as_error() {
        // A declined/canceled approval completes the item with exit_code:null but
        // never ran — must surface as an error, not a (default) success.
        for status in ["declined", "canceled", "cancelled"] {
            let l = format!(
                r#"{{"type":"item.completed","item":{{"id":"i","type":"command_execution","command":"rm -rf /","aggregated_output":"","exit_code":null,"status":"{status}"}}}}"#
            );
            match parse_line_for("codex", &l) {
                ChatEvent::ToolResults { items } => assert!(items[0].is_error, "{status}"),
                e => panic!("{status}: {e:?}"),
            }
        }
    }

    #[test]
    fn codex_mcp_object_result_is_serialized() {
        // Weft's MCP tools return result objects ({content:[{text}]}); the exec row
        // must render that text (or serialize), not complete with empty output.
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"m","type":"mcp_tool_call","tool":"weft_planner__get_task","result":{"content":[{"type":"text","text":"task #3"}]},"status":"completed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => assert_eq!(items[0].output, "task #3"),
            e => panic!("{e:?}"),
        }
        // a non-content object result is serialized rather than dropped.
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"m2","type":"mcp_tool_call","tool":"x","result":{"ok":true},"status":"completed"}}"#,
        ) {
            ChatEvent::ToolResults { items } => assert_eq!(items[0].output, r#"{"ok":true}"#),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn codex_plan_item_surfaces_its_text() {
        // /plan and /review content items carry text only on completion (no deltas);
        // surface it as assistant text rather than dropping it.
        match parse_line_for(
            "codex",
            r#"{"type":"item.completed","item":{"id":"p","type":"plan","text":"1. do x\n2. do y","status":"completed"}}"#,
        ) {
            ChatEvent::Assistant { texts, tools, .. } => {
                assert_eq!(texts, vec!["1. do x\n2. do y".to_string()]);
                assert!(tools.is_empty());
            }
            e => panic!("{e:?}"),
        }
        // the running half (started) opens no row.
        assert!(matches!(
            parse_line_for(
                "codex",
                r#"{"type":"item.started","item":{"id":"p","type":"plan","status":"in_progress"}}"#,
            ),
            ChatEvent::Other
        ));
    }

    #[test]
    fn codex_ignores_non_tool_items() {
        // plan/review/other content items aren't tool calls — they must not open
        // an empty tool row (started) or a stray result (completed).
        for ty in ["plan", "review", "todo_list", "web_search"] {
            let started = format!(
                r#"{{"type":"item.started","item":{{"id":"x","type":"{ty}","status":"in_progress"}}}}"#
            );
            assert!(
                matches!(parse_line_for("codex", &started), ChatEvent::Other),
                "started {ty}"
            );
            let completed = format!(
                r#"{{"type":"item.completed","item":{{"id":"x","type":"{ty}","status":"completed"}}}}"#
            );
            assert!(
                matches!(parse_line_for("codex", &completed), ChatEvent::Other),
                "completed {ty}"
            );
        }
    }

    #[test]
    fn codex_turn_completed_does_not_report_cumulative_usage_as_context() {
        // exec's input_tokens is cumulative session throughput, not current context
        // (verified live), so we report None rather than a value that climbs to 100%.
        let l = r#"{"type":"turn.completed","usage":{"input_tokens":47163,"cached_input_tokens":27392,"output_tokens":284}}"#;
        match parse_line_for("codex", l) {
            ChatEvent::TurnEnd { context_tokens, is_error } => {
                assert!(!is_error);
                assert_eq!(context_tokens, None);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_opencode_dialect() {
        let txt = r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"done"}}"#;
        assert_eq!(extract_native("opencode", txt).as_deref(), Some("ses_1"));
        match parse_line_for("opencode", txt) {
            ChatEvent::Assistant { texts, .. } => assert_eq!(texts, vec!["done"]),
            e => panic!("{e:?}"),
        }
        match parse_line_for(
            "opencode",
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\n"}}}"#,
        ) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0].name, "bash");
                assert_eq!(tools[0].summary, "echo hi");
                assert_eq!(tools[0].input["command"], "echo hi");
                assert_eq!(tools[0].output.as_deref(), Some("hi\n"));
            }
            e => panic!("{e:?}"),
        }
        // running frames carry no result yet and have no stable id to dedupe, so
        // they are skipped — only the completed row lands.
        assert!(matches!(
            parse_line_for(
                "opencode",
                r#"{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"echo hi"}}}}"#,
            ),
            ChatEvent::Other
        ));
        assert!(matches!(
            parse_line_for("opencode", r#"{"type":"step_start","part":{}}"#),
            ChatEvent::Other
        ));
    }

    #[test]
    fn parses_initialize_commands() {
        let l = r#"{"type":"control_response","response":{"subtype":"success","request_id":"weft-init","response":{"commands":[{"name":"compact","description":"x"},{"name":"superpowers:requesting-code-review"}]}}}"#;
        match parse_line(l) {
            ChatEvent::Commands { commands } => {
                assert_eq!(
                    commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                    vec!["compact", "superpowers:requesting-code-review"]
                );
                assert_eq!(commands[0].description.as_deref(), Some("x"));
            }
            e => panic!("{e:?}"),
        }
        // interrupt acks (no commands payload) stay Other
        let ack = r#"{"type":"control_response","response":{"subtype":"success","request_id":"weft-int-1"}}"#;
        assert!(matches!(parse_line(ack), ChatEvent::Other));
    }

    #[test]
    fn parses_result_and_garbage() {
        assert!(matches!(
            parse_line(r#"{"type":"result","subtype":"success","is_error":false}"#),
            ChatEvent::TurnEnd { is_error: false, .. }
        ));
        assert!(matches!(
            parse_line(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#),
            ChatEvent::TurnEnd { is_error: true, .. }
        ));
        assert!(matches!(parse_line("not json"), ChatEvent::Other));
        assert!(matches!(
            parse_line(r#"{"type":"system","subtype":"hook_started"}"#),
            ChatEvent::Other
        ));
        assert!(matches!(
            parse_line(r#"{"type":"rate_limit_event"}"#),
            ChatEvent::Other
        ));
    }
}
