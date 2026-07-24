//! Map ACP `session/update` payloads onto Weft `ChatEvent` / side channels.
//!
//! Backend-agnostic: no CLI identity strings. Thinking chunks are dropped in v1.

use serde_json::Value;

use crate::lead_chat::proto::{ChatEvent, SlashCmd, ToolCall, ToolResultItem};

/// Outcome of mapping one `update` object (the inner `params.update`).
#[derive(Debug)]
pub enum UpdateOut {
    Chat(ChatEvent),
    Commands(Vec<SlashCmd>),
    Usage {
        context_tokens: u64,
        window: Option<u64>,
    },
    Meta {
        model: Option<String>,
        thinking: Option<String>,
    },
    Ignore,
}

/// Map a single ACP `update` value.
pub fn update_to_out(update: &Value) -> UpdateOut {
    let Some(kind) = update.get("sessionUpdate").and_then(|k| k.as_str()) else {
        return UpdateOut::Ignore;
    };
    match kind {
        "agent_message_chunk" => text_delta(update),
        "agent_thought_chunk" => UpdateOut::Ignore,
        "user_message_chunk" => UpdateOut::Ignore,
        "tool_call" => tool_call_start(update),
        "tool_call_update" => tool_call_update(update),
        "available_commands_update" => commands(update),
        "usage_update" => usage(update),
        "config_option_update" => config_meta(update),
        "session_info_update" | "current_mode_update" | "plan" | "plan_update" | "plan_removed" => {
            UpdateOut::Ignore
        }
        _ => UpdateOut::Ignore,
    }
}

fn text_delta(update: &Value) -> UpdateOut {
    let text = update
        .pointer("/content/text")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return UpdateOut::Ignore;
    }
    UpdateOut::Chat(ChatEvent::TextDelta {
        text: text.to_string(),
        item: None,
    })
}

fn tool_call_start(update: &Value) -> UpdateOut {
    let id = update
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return UpdateOut::Ignore;
    }
    let title = update
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = update
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");
    // Prefer a concrete tool name when rawInput implies one; else kind/title.
    let name = tool_name_from_raw(update.get("rawInput"), kind);
    let input = update
        .get("rawInput")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let summary = if title.is_empty() {
        name.clone()
    } else {
        title
    };
    UpdateOut::Chat(ChatEvent::Assistant {
        texts: vec![],
        tools: vec![ToolCall {
            id,
            name,
            input,
            summary,
            output: None,
            is_error: false,
        }],
        uuid: None,
    })
}

fn tool_name_from_raw(raw: Option<&Value>, kind: &str) -> String {
    let Some(raw) = raw else {
        return kind.to_string();
    };
    if raw.get("command").and_then(|c| c.as_str()).is_some() {
        return "bash".into();
    }
    if raw.get("path").is_some() || raw.get("file_path").is_some() {
        if kind == "edit" || kind == "delete" || kind == "move" {
            return kind.to_string();
        }
        return "read".into();
    }
    kind.to_string()
}

fn tool_call_update(update: &Value) -> UpdateOut {
    let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");
    // Only terminal states become ToolResults; in_progress is Ignore (start row exists).
    if status != "completed" && status != "failed" {
        return UpdateOut::Ignore;
    }
    let id = update
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return UpdateOut::Ignore;
    }
    let output = extract_tool_output(update);
    let is_error = status == "failed";
    UpdateOut::Chat(ChatEvent::ToolResults {
        items: vec![ToolResultItem {
            id,
            output,
            is_error,
        }],
    })
}

fn extract_tool_output(update: &Value) -> String {
    // Prefer rawOutput.content text blocks; fall back to content[] text tails.
    if let Some(arr) = update.pointer("/rawOutput/content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            }
        }
        if !parts.is_empty() {
            return parts.join("");
        }
    }
    if let Some(arr) = update.get("content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(t) = item.pointer("/content/text").and_then(|t| t.as_str()) {
                // Skip the leading "$ cmd" presentation block when a later block exists.
                parts.push(t.to_string());
            } else if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            }
        }
        if parts.len() > 1 {
            // Drop first if it looks like a command echo.
            if parts[0].starts_with("$ ") {
                return parts[1..].join("");
            }
        }
        if !parts.is_empty() {
            return parts.join("");
        }
    }
    String::new()
}

fn commands(update: &Value) -> UpdateOut {
    let Some(arr) = update
        .get("availableCommands")
        .and_then(|a| a.as_array())
    else {
        return UpdateOut::Ignore;
    };
    let mut out = Vec::new();
    for c in arr {
        let Some(name) = c.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let description = c
            .get("description")
            .and_then(|d| d.as_str())
            .map(str::to_string);
        let arg_hint = c
            .pointer("/input/hint")
            .and_then(|h| h.as_str())
            .map(str::to_string);
        out.push(SlashCmd {
            name: name.to_string(),
            description,
            arg_hint,
        });
    }
    if out.is_empty() {
        UpdateOut::Ignore
    } else {
        UpdateOut::Commands(out)
    }
}

fn usage(update: &Value) -> UpdateOut {
    let used = update
        .get("used")
        .and_then(|u| u.as_u64())
        .or_else(|| update.get("used").and_then(|u| u.as_i64()).map(|i| i as u64));
    let Some(context_tokens) = used else {
        return UpdateOut::Ignore;
    };
    let window = update
        .get("size")
        .and_then(|s| s.as_u64())
        .or_else(|| update.get("size").and_then(|s| s.as_i64()).map(|i| i as u64));
    UpdateOut::Usage {
        context_tokens,
        window,
    }
}

fn config_meta(update: &Value) -> UpdateOut {
    // config_option_update may carry currentValue for model/thinking ids.
    let id = update.get("configId").and_then(|i| i.as_str()).unwrap_or("");
    let val = update
        .get("currentValue")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if id == "model" {
        return UpdateOut::Meta {
            model: val,
            thinking: None,
        };
    }
    if id == "thinking" {
        return UpdateOut::Meta {
            model: None,
            thinking: val,
        };
    }
    UpdateOut::Ignore
}

/// Map `session/prompt` stopReason → TurnEnd-ish flags.
pub fn stop_reason_is_error(stop: &str) -> bool {
    matches!(stop, "refusal" | "max_tokens" | "max_turn_requests")
}

pub fn stop_reason_is_cancelled(stop: &str) -> bool {
    stop == "cancelled"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_chunk_to_text_delta() {
        let u = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "pong" }
        });
        match update_to_out(&u) {
            UpdateOut::Chat(ChatEvent::TextDelta { text, item: None }) => {
                assert_eq!(text, "pong");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn thought_chunk_ignored() {
        let u = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" }
        });
        assert!(matches!(update_to_out(&u), UpdateOut::Ignore));
    }

    #[test]
    fn tool_call_and_completed_from_fixture_shapes() {
        let start = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-1",
            "title": "$ echo TOOL_OK",
            "kind": "execute",
            "status": "pending",
            "rawInput": { "command": "echo TOOL_OK" }
        });
        match update_to_out(&start) {
            UpdateOut::Chat(ChatEvent::Assistant { tools, .. }) => {
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].id, "call-1");
                assert_eq!(tools[0].name, "bash");
                assert_eq!(tools[0].summary, "$ echo TOOL_OK");
                assert_eq!(tools[0].input["command"], "echo TOOL_OK");
            }
            o => panic!("{o:?}"),
        }

        let done = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "completed",
            "rawOutput": {
                "content": [{ "type": "text", "text": "TOOL_OK\n" }]
            }
        });
        match update_to_out(&done) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, "call-1");
                assert!(items[0].output.contains("TOOL_OK"));
                assert!(!items[0].is_error);
            }
            o => panic!("{o:?}"),
        }

        let inflight = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "in_progress"
        });
        assert!(matches!(update_to_out(&inflight), UpdateOut::Ignore));
    }

    #[test]
    fn usage_update_maps_size_used() {
        let u = json!({ "sessionUpdate": "usage_update", "size": 500000, "used": 25615 });
        match update_to_out(&u) {
            UpdateOut::Usage {
                context_tokens,
                window: Some(w),
            } => {
                assert_eq!(context_tokens, 25615);
                assert_eq!(w, 500000);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn available_commands_update() {
        let u = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "model", "description": "Show model" },
                { "name": "fast", "description": "Toggle fast", "input": { "hint": "[on|off]" } }
            ]
        });
        match update_to_out(&u) {
            UpdateOut::Commands(cmds) => {
                assert_eq!(cmds.len(), 2);
                assert_eq!(cmds[0].name, "model");
                assert_eq!(cmds[1].arg_hint.as_deref(), Some("[on|off]"));
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn stop_reason_helpers() {
        assert!(!stop_reason_is_error("end_turn"));
        assert!(stop_reason_is_error("refusal"));
        assert!(stop_reason_is_cancelled("cancelled"));
    }
}
