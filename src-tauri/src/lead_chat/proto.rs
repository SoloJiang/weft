//! Parse one stdout line of `claude -p --output-format stream-json` into the
//! few shapes the chat engine cares about. Unknown lines (hooks, rate-limit
//! events, thinking/signature deltas …) are Other and ignored. Shapes verified
//! against a live CLI (2.1.x) spike — see the design spec §1.

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
    Other,
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
                .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
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
            for b in v["message"]["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
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
        _ => ChatEvent::Other,
    }
}

/// First string-ish field of a tool input, truncated — just enough for a
/// compact activity pill. An empty/opaque input yields "" (render nothing),
/// never "{}" noise.
fn compact_input(input: &Value) -> String {
    let s = ["file_path", "path", "command", "pattern", "query", "url", "description"]
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
            ChatEvent::Init { session_id, slash_commands } => {
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
        assert!(matches!(parse_line(r#"{"type":"rate_limit_event"}"#), ChatEvent::Other));
    }
}
