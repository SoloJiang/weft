//! Map Weft allow/deny decisions onto ACP `session/request_permission` options.
//! Session-scoped always-cache only — no durable grants here.

use std::collections::HashMap;

use serde_json::Value;

/// Which permission option class we want to select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl Want {
    fn kind_str(self) -> &'static str {
        match self {
            Want::AllowOnce => "allow_once",
            Want::AllowAlways => "allow_always",
            Want::RejectOnce => "reject_once",
            Want::RejectAlways => "reject_always",
        }
    }

    pub fn is_allow(self) -> bool {
        matches!(self, Want::AllowOnce | Want::AllowAlways)
    }
}

/// Pick an `optionId` from the agent's offered options for `want`.
/// Prefers matching `kind`, then `optionId` string equality with the kind name.
pub fn pick_option_id(options: &[Value], want: Want) -> Option<String> {
    let kind = want.kind_str();
    for opt in options {
        if opt.get("kind").and_then(|k| k.as_str()) == Some(kind) {
            if let Some(id) = opt.get("optionId").and_then(|i| i.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    for opt in options {
        if opt.get("optionId").and_then(|i| i.as_str()) == Some(kind) {
            return Some(kind.to_string());
        }
    }
    // Soft fallback: first allow_* / reject_* by prefix.
    let prefix = if want.is_allow() { "allow" } else { "reject" };
    for opt in options {
        let id = opt.get("optionId").and_then(|i| i.as_str()).unwrap_or("");
        let k = opt.get("kind").and_then(|i| i.as_str()).unwrap_or("");
        if id.starts_with(prefix) || k.starts_with(prefix) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Build the ACP permission result body for a selected optionId.
pub fn selected_outcome(option_id: &str) -> Value {
    serde_json::json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    })
}

/// Stable cache key for always-allow / always-deny within one ACP session.
pub fn intent_key(tool_kind: &str, raw_input: &Value) -> String {
    if let Some(cmd) = raw_input.get("command").and_then(|c| c.as_str()) {
        // Coarse: tool family only for always (matches omp cacheKey: toolName).
        // Keep command out so "always allow bash" is session-wide for bash.
        let _ = cmd;
        return format!("{tool_kind}:bash");
    }
    if tool_kind == "edit" || tool_kind == "delete" || tool_kind == "move" {
        return format!("edit:{tool_kind}");
    }
    if let Some(k) = raw_input.get("kind").and_then(|k| k.as_str()) {
        return format!("{tool_kind}:{k}");
    }
    tool_kind.to_string()
}

/// Intent key from a full permission request params object.
pub fn intent_key_from_params(params: &Value) -> String {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = tc
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("tool");
    let raw = tc.get("rawInput").cloned().unwrap_or(Value::Null);
    intent_key(kind, &raw)
}

/// Human summary line for the Needs-you card.
pub fn summary_from_params(params: &Value) -> (String, String) {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    let title = tc
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("tool");
    let detail = tc
        .get("rawInput")
        .map(|r| r.to_string())
        .unwrap_or_default();
    let summary = if title.is_empty() {
        "Permission required".into()
    } else {
        title.to_string()
    };
    (summary, detail)
}

#[derive(Debug, Default, Clone)]
pub struct AlwaysCache {
    map: HashMap<String, Want>,
}

impl AlwaysCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Want> {
        self.map.get(key).copied()
    }

    pub fn set(&mut self, key: String, want: Want) {
        if matches!(want, Want::AllowAlways | Want::RejectAlways) {
            self.map.insert(key, want);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_options() -> Vec<Value> {
        vec![
            json!({"optionId":"allow_once","name":"Allow once","kind":"allow_once"}),
            json!({"optionId":"allow_always","name":"Always allow","kind":"allow_always"}),
            json!({"optionId":"reject_once","name":"Reject","kind":"reject_once"}),
            json!({"optionId":"reject_always","name":"Always reject","kind":"reject_always"}),
        ]
    }

    #[test]
    fn picks_by_kind() {
        let opts = sample_options();
        assert_eq!(
            pick_option_id(&opts, Want::AllowOnce).as_deref(),
            Some("allow_once")
        );
        assert_eq!(
            pick_option_id(&opts, Want::RejectAlways).as_deref(),
            Some("reject_always")
        );
    }

    #[test]
    fn selected_outcome_shape() {
        let v = selected_outcome("allow_once");
        assert_eq!(v["outcome"]["outcome"], "selected");
        assert_eq!(v["outcome"]["optionId"], "allow_once");
    }

    #[test]
    fn intent_key_bash_is_coarse() {
        let k = intent_key("execute", &json!({"command": "echo hi"}));
        assert_eq!(k, "execute:bash");
        let k2 = intent_key("execute", &json!({"command": "rm -rf /"}));
        assert_eq!(k, k2, "always-allow is session-wide for bash family");
    }

    #[test]
    fn always_cache_only_stores_always_variants() {
        let mut c = AlwaysCache::new();
        c.set("k".into(), Want::AllowOnce);
        assert!(c.get("k").is_none());
        c.set("k".into(), Want::AllowAlways);
        assert_eq!(c.get("k"), Some(Want::AllowAlways));
    }

    #[test]
    fn summary_from_permission_fixture_shape() {
        let params = json!({
            "sessionId": "s",
            "toolCall": {
                "toolCallId": "c1",
                "title": "echo TOOL_OK",
                "kind": "execute",
                "rawInput": { "command": "echo TOOL_OK" }
            }
        });
        let (s, _) = summary_from_params(&params);
        assert_eq!(s, "echo TOOL_OK");
        assert_eq!(intent_key_from_params(&params), "execute:bash");
    }
}
