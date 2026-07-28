//! Pure JSON-RPC 2.0 framing for the Agent Client Protocol.
//!
//! Real `"jsonrpc":"2.0"` envelopes (unlike Codex app-server's jsonrpc-like
//! dialect). This module has no process I/O and no backend-specific strings.

use serde_json::{json, Map, Value};

/// A classified inbound line from an ACP agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Reply to one of our requests — correlate by integer id.
    Response { id: i64, result: Value },
    /// Error reply to one of our requests.
    Error { id: i64, code: i64, message: String },
    /// Server → client notification (no id), e.g. `session/update`.
    Notification { method: String, params: Value },
    /// Server → client request that must be answered, id echoed verbatim
    /// (may be integer **or** string — omp has used `id: 0`).
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// Malformed / non-jsonrpc line.
    Skip,
}

/// Encode a client → agent request line (newline-terminated).
pub fn encode_request(id: i64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
    )
}

/// Encode a client → agent notification (no id), e.g. `session/cancel`.
pub fn encode_notification(method: &str, params: Option<Value>) -> String {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), Value::String("2.0".into()));
    obj.insert("method".into(), Value::String(method.into()));
    if let Some(p) = params {
        obj.insert("params".into(), p);
    }
    format!("{}\n", Value::Object(obj))
}

/// Encode our reply to a server-initiated request — echo `id` verbatim.
pub fn encode_response(id: &Value, result: Value) -> String {
    format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
    )
}

/// Encode an error reply to a server-initiated request.
pub fn encode_error_response(id: &Value, code: i64, message: &str) -> String {
    format!(
        "{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
    )
}

/// Classify one stdout line.
pub fn classify(line: &str) -> Incoming {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Incoming::Skip;
    };
    let Some(obj) = v.as_object() else {
        return Incoming::Skip;
    };
    if obj.get("jsonrpc").and_then(|j| j.as_str()) != Some("2.0") {
        return Incoming::Skip;
    }

    let id = obj.get("id");
    let method = obj.get("method").and_then(|m| m.as_str());
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");

    // Response / error to our request: id + result|error, no method.
    if let Some(id_v) = id {
        if method.is_none() {
            if has_result {
                let Some(rid) = id_as_i64(id_v) else {
                    return Incoming::Skip;
                };
                return Incoming::Response {
                    id: rid,
                    result: obj.get("result").cloned().unwrap_or(Value::Null),
                };
            }
            if has_error {
                let Some(rid) = id_as_i64(id_v) else {
                    return Incoming::Skip;
                };
                let err = obj.get("error").cloned().unwrap_or(Value::Null);
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                return Incoming::Error {
                    id: rid,
                    code,
                    message,
                };
            }
        }
        // Server request: method + id, no result/error.
        if let Some(m) = method {
            if !has_result && !has_error {
                return Incoming::ServerRequest {
                    id: id_v.clone(),
                    method: m.to_string(),
                    params: obj.get("params").cloned().unwrap_or(Value::Null),
                };
            }
        }
    }

    // Notification: method, no id.
    if let Some(m) = method {
        if id.is_none() {
            return Incoming::Notification {
                method: m.to_string(),
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            };
        }
    }

    Incoming::Skip
}

fn id_as_i64(id: &Value) -> Option<i64> {
    if let Some(n) = id.as_i64() {
        return Some(n);
    }
    if let Some(n) = id.as_u64() {
        return i64::try_from(n).ok();
    }
    if let Some(s) = id.as_str() {
        return s.parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_initialize_result_fixture_shape() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#;
        match classify(line) {
            Incoming::Response { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result["protocolVersion"], 1);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn classifies_permission_server_request_id_zero() {
        let line = r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{}}"#;
        match classify(line) {
            Incoming::ServerRequest { id, method, .. } => {
                assert_eq!(id, json!(0));
                assert_eq!(method, "session/request_permission");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn classifies_session_update_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{}}}"#;
        match classify(line) {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "s");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn classifies_error_response() {
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"Invalid params"}}"#;
        match classify(line) {
            Incoming::Error { id, code, message } => {
                assert_eq!(id, 7);
                assert_eq!(code, -32602);
                assert_eq!(message, "Invalid params");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn skips_non_jsonrpc() {
        assert_eq!(classify(r#"{"id":1,"result":{}}"#), Incoming::Skip);
        assert_eq!(classify("not-json"), Incoming::Skip);
    }

    #[test]
    fn encode_request_is_newline_terminated_jsonrpc() {
        let s = encode_request(3, "session/new", json!({"cwd": "/tmp"}));
        assert!(s.ends_with('\n'));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 3);
        assert_eq!(v["method"], "session/new");
    }

    #[test]
    fn encode_response_echoes_id_verbatim() {
        let s = encode_response(&json!(0), json!({"ok": true}));
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["id"], 0);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn encode_notification_session_cancel() {
        let s = encode_notification(
            "session/cancel",
            Some(json!({"sessionId": "abc"})),
        );
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert!(v.get("id").is_none());
        assert_eq!(v["method"], "session/cancel");
        assert_eq!(v["params"]["sessionId"], "abc");
    }
}
