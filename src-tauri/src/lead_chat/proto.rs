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

/// A tool invocation captured from the stream. Carries enough to render an
/// expandable row — the full `input` plus a compact `summary` for the collapsed
/// line. `output` is set only when the dialect delivers the result inline
/// (opencode's completed `tool_use`); claude sends results separately as
/// `ToolResults`, and codex builds pill-only calls (empty `id`, no `input`).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub summary: String,
    pub output: Option<String>,
    pub is_error: bool,
}

impl ToolCall {
    /// Pill-only call (codex exec items): name + compact summary, no id/io — the
    /// engine renders these as a transient activity pill, not a persisted row.
    pub(crate) fn pill(name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            name: name.into(),
            input: Value::Null,
            summary: summary.into(),
            output: None,
            is_error: false,
        }
    }
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
    },
    TextDelta {
        text: String,
    },
    /// One complete assistant message event: its text blocks plus any tool calls
    /// it started. Codex builds pill-only calls (transient activity); claude and
    /// opencode build full calls persisted as expandable tool rows.
    Assistant {
        texts: Vec<String>,
        tools: Vec<ToolCall>,
    },
    /// Tool results delivered out-of-band (claude `user` message), each
    /// correlated to its `Assistant` tool call by id.
    ToolResults {
        items: Vec<ToolResultItem>,
    },
    TurnEnd {
        is_error: bool,
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
                        tools: vec![ToolCall::pill(
                            other,
                            summary.chars().take(120).collect::<String>(),
                        )],
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
                    input,
                    summary,
                    output: Some(opencode_output(state)),
                    is_error: status == "error",
                }],
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
                            input,
                            summary,
                            output: None,
                            is_error: false,
                        });
                    }
                    _ => {}
                }
            }
            ChatEvent::Assistant { texts, tools }
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

    #[test]
    fn parses_init() {
        let l = r#"{"type":"system","subtype":"init","session_id":"abc-123","slash_commands":["compact","commit"]}"#;
        match parse_line(l) {
            ChatEvent::Init {
                session_id,
                slash_commands,
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
        let l = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#;
        match parse_line(l) {
            ChatEvent::Assistant { texts, tools } => {
                assert_eq!(texts, vec!["done"]);
                assert_eq!(tools[0].id, "toolu_1");
                assert_eq!(tools[0].name, "Read");
                assert!(tools[0].summary.contains("b.rs"));
                // full input is kept for the expandable row, not just the summary
                assert_eq!(tools[0].input["file_path"], "/a/b.rs");
                assert!(tools[0].output.is_none());
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
                assert_eq!(tools[0].name, "command_execution");
                assert_eq!(tools[0].summary, "npm test");
                // codex stays a transient pill: no correlation id, no persisted io
                assert!(tools[0].id.is_empty());
                assert!(tools[0].output.is_none());
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
