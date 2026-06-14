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
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct McpServer {
    pub name: String,
    pub status: String, // connected | pending | failed | …(原样透传)
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
    /// One complete assistant message event: its text blocks + (tool name,
    /// compact summary) pairs. The CLI emits one per finished content block.
    Assistant {
        texts: Vec<String>,
        tools: Vec<(String, String)>,
    },
    TurnEnd {
        is_error: bool,
        /// 当前上下文 token(input + cache_read + cache_creation);拿不到为 None。
        context_tokens: Option<u64>,
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
    match v["type"].as_str() {
        Some("item.completed") | Some("item.started") => {
            let item = &v["item"];
            match item["type"].as_str() {
                Some("agent_message") if v["type"] == "item.completed" => ChatEvent::Assistant {
                    texts: item["text"]
                        .as_str()
                        .map(|t| vec![t.to_string()])
                        .unwrap_or_default(),
                    tools: vec![],
                },
                Some("agent_message") => ChatEvent::Other,
                Some("error") => ChatEvent::TextDelta {
                    text: error_text_from_item(item),
                },
                Some(other) => {
                    // command_execution / file_change / mcp_tool_call / reasoning…
                    if other == "reasoning" {
                        return ChatEvent::Other;
                    }
                    let summary = ["command", "text", "name", "path"]
                        .iter()
                        .find_map(|k| item[k].as_str())
                        .unwrap_or_default();
                    ChatEvent::Assistant {
                        texts: vec![],
                        tools: vec![(other.to_string(), summary.chars().take(120).collect())],
                    }
                }
                None => ChatEvent::Other,
            }
        }
        Some("turn.completed") => ChatEvent::TurnEnd {
            is_error: false,
            // codex usage.input_tokens 已含 cached 子集,即本回合送入的上下文体量。
            context_tokens: v["usage"]["input_tokens"].as_u64(),
        },
        Some("turn.failed") | Some("error") => ChatEvent::TurnEnd {
            is_error: true,
            context_tokens: None,
        },
        _ => ChatEvent::Other,
    }
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
        },
        Some("tool_use") => ChatEvent::Assistant {
            texts: vec![],
            tools: vec![(
                part["tool"].as_str().unwrap_or("tool").to_string(),
                compact_input(&part["state"]["input"]),
            )],
        },
        _ => ChatEvent::Other,
    }
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
    text.trim().to_string()
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
                    Some("tool_use") => tools.push((
                        b["name"].as_str().unwrap_or("tool").to_string(),
                        compact_input(&b["input"]),
                    )),
                    _ => {}
                }
            }
            ChatEvent::Assistant { texts, tools }
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

#[cfg(test)]
mod tests {
    use super::*;

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
                assert_eq!(tools[0].0, "mcp__weft_planner__get_task");
                assert_eq!(tools[0].1, "");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parses_assistant_blocks() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"},{"type":"tool_use","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { texts, tools } => {
                assert_eq!(texts, vec!["done"]);
                assert_eq!(tools[0].0, "Read");
                assert!(tools[0].1.contains("b.rs"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn thinking_only_assistant_is_empty() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"...","signature":"s"}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { texts, tools } => {
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
        match parse_line_for(
            "codex",
            r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test"}}"#,
        ) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0], ("command_execution".into(), "npm test".into()))
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
    fn codex_turn_completed_carries_context_tokens() {
        let l = r#"{"type":"turn.completed","usage":{"input_tokens":47163,"cached_input_tokens":27392,"output_tokens":284}}"#;
        match parse_line_for("codex", l) {
            ChatEvent::TurnEnd { context_tokens, is_error } => {
                assert!(!is_error);
                assert_eq!(context_tokens, Some(47163)); // input_tokens 已含 cached 子集
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
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"echo hi"}}}}"#,
        ) {
            ChatEvent::Assistant { tools, .. } => {
                assert_eq!(tools[0], ("bash".into(), "echo hi".into()))
            }
            e => panic!("{e:?}"),
        }
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
