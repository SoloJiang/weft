//! Codex app-server events normalized for the orchestrator and curator.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub summary: String,
    pub output: Option<String>,
    pub is_error: bool,
    pub collab_threads: Vec<String>,
    pub images: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ToolResultItem {
    pub id: String,
    pub output: String,
    pub is_error: bool,
    pub collab_threads: Vec<String>,
    pub images: Vec<String>,
}

#[derive(Debug)]
pub enum ChatEvent {
    TextDelta {
        text: String,
        item: Option<String>,
        agent_thread: Option<String>,
    },
    TextDone {
        item: Option<String>,
        text: Option<String>,
        agent_thread: Option<String>,
    },
    Assistant {
        texts: Vec<String>,
        tools: Vec<ToolCall>,
        /// Reserved native anchor; Codex app-server currently leaves it empty.
        uuid: Option<String>,
        agent_thread: Option<String>,
    },
    ToolResults {
        items: Vec<ToolResultItem>,
    },
    TurnEnd {
        is_error: bool,
        context_tokens: Option<u64>,
    },
    Usage {
        context_tokens: u64,
        window: Option<u64>,
    },
}

/// Display text of a non-tool content item such as plan or review output.
pub(crate) fn codex_content_item_text(item: &Value) -> Option<String> {
    ["text", "review", "plan", "message"]
        .iter()
        .find_map(|key| {
            let value = item[*key].as_str()?.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
}

/// Cap structured tool input before it enters an event payload.
pub(crate) fn cap_input(input: Value) -> Value {
    const MAX_CHARS: usize = 16_000;

    let serialized = input.to_string();
    if serialized.chars().count() <= MAX_CHARS {
        return input;
    }
    let mut capped: String = serialized.chars().take(MAX_CHARS).collect();
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

/// Extract a useful message when Codex forwards a provider JSON error as text.
pub(crate) fn humanize_error_text(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return trimmed.to_string();
    }
    let mut objects = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    if let Some(Ok(value)) = objects.next() {
        let message = value["error"]["message"]
            .as_str()
            .or_else(|| value["message"].as_str())
            .or_else(|| value["error"].as_str())
            .map(str::trim)
            .filter(|message| !message.is_empty());
        if let Some(message) = message {
            return message.to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn caps_large_tool_input_but_preserves_small_objects() {
        let small = json!({"path": "src/main.rs"});
        assert_eq!(cap_input(small.clone()), small);

        let large = json!({"content": "x".repeat(20_000)});
        let capped = cap_input(large);
        let text = capped.as_str().expect("large input becomes text");
        assert!(text.chars().count() < 16_100);
        assert!(text.ends_with("… (truncated)"));
    }

    #[test]
    fn content_item_uses_the_first_non_empty_text_field() {
        assert_eq!(
            codex_content_item_text(&json!({"text": " ", "plan": "Ship it"})).as_deref(),
            Some("Ship it")
        );
        assert!(codex_content_item_text(&json!({"status": "completed"})).is_none());
    }

    #[test]
    fn humanizes_nested_provider_errors() {
        assert_eq!(
            humanize_error_text(r#"{"error":{"message":"invalid request"}}"#),
            "invalid request"
        );
        assert_eq!(humanize_error_text("plain failure"), "plain failure");
    }
}
