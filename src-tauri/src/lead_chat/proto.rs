//! Parse one stdout line of a headless agent CLI into the few shapes the chat
//! engine cares about. Three dialects, all spike-verified live:
//! - claude: `-p --output-format stream-json` (long-lived, deltas + result)
//! - codex: `exec --json` (per-turn; thread.started / item.* / turn.completed)
//! - opencode: `run --format json` (per-turn; text / tool_use, EOF ends turn)
//! Unknown lines are Other and ignored.

use serde_json::Value;

#[derive(Debug)]
pub enum ChatEvent {
    Init {
        session_id: String,
        slash_commands: Vec<String>,
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
    },
    /// Response to our `initialize` control_request: the CLI's slash commands.
    /// Sent right after spawn — the `init` system message only arrives with the
    /// FIRST user turn, far too late for the composer's palette.
    Commands {
        commands: Vec<String>,
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
        Some("turn.completed") => ChatEvent::TurnEnd { is_error: false },
        Some("turn.failed") | Some("error") => ChatEvent::TurnEnd { is_error: true },
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
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
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
        },
        Some("control_response") => {
            let r = &v["response"];
            if r["subtype"] == "success" {
                if let Some(cmds) = r["response"]["commands"].as_array() {
                    return ChatEvent::Commands {
                        commands: cmds
                            .iter()
                            .filter_map(|c| c["name"].as_str().map(String::from))
                            .collect(),
                    };
                }
            }
            ChatEvent::Other
        }
        _ => ChatEvent::Other,
    }
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
            } => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(slash_commands, vec!["compact", "commit"]);
            }
            e => panic!("{e:?}"),
        }
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
        assert!(matches!(
            parse_line_for("codex", r#"{"type":"turn.completed","usage":{}}"#),
            ChatEvent::TurnEnd { is_error: false }
        ));
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
                    commands,
                    vec!["compact", "superpowers:requesting-code-review"]
                );
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
            ChatEvent::TurnEnd { is_error: false }
        ));
        assert!(matches!(
            parse_line(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#),
            ChatEvent::TurnEnd { is_error: true }
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
