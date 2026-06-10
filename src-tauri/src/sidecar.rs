//! Sidecar: read a tool's own session transcript and normalize it into clean,
//! app-native events (NormEvent) for the observe-mode chat view — so the common
//! "watch the agent" case never depends on a live process.
//!
//! Claude: jsonl under ~/.claude/projects/<encoded-cwd>/.
//! Codex: rollout jsonl under ~/.codex/sessions/<date>/, located by matching the
//! session_meta cwd.
//! OpenCode: messages in the SQLite db (~/.local/share/opencode/opencode.db),
//! located by session.directory; read read-only so the live (WAL) db is safe.

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DbBackend, Statement};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A normalized, tool-agnostic transcript event for the chat view.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NormEvent {
    /// A conversation turn (human or agent prose).
    Message { role: String, text: String, ts: String },
    /// An agent tool call, summarized to one line.
    Tool { name: String, summary: String, ts: String },
}

/// Read the full normalized transcript for a session's cwd. Best-effort: an
/// unreadable / not-yet-created file yields an empty list, not an error.
pub async fn read_transcript(cwd: &Path, tool: &str) -> Vec<NormEvent> {
    match tool {
        "claude" => read_claude(cwd).unwrap_or_default(),
        "codex" => read_codex(cwd).unwrap_or_default(),
        "opencode" => read_opencode(cwd).await.unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Strip MCP server prefixes so tool pills read cleanly:
/// `mcp__weft_bus__bus_post` / `weft_bus_bus_post` → `bus_post`.
fn clean_tool_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return rest.rsplit("__").next().unwrap_or(rest).to_string();
    }
    for p in ["weft_bus_", "weft_planner_"] {
        if let Some(rest) = name.strip_prefix(p) {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// One-line, truncated summary string for a tool call.
fn truncate(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() > 80 {
        line.chars().take(80).collect::<String>() + "…"
    } else {
        line.to_string()
    }
}

fn read_claude(cwd: &Path) -> Option<Vec<NormEvent>> {
    let dir = crate::claude::projects_dir_for(cwd).ok()?;
    // Newest *.jsonl in the project dir is this cwd's active session.
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        // Skip a file whose metadata momentarily fails (claude mid-write) rather
        // than aborting the whole read — aborting would blank the transcript.
        let Ok(mt) = std::fs::metadata(&p).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map_or(true, |(bm, _)| mt >= *bm) {
            best = Some((mt, p));
        }
    }
    let (_, path) = best?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            parse_claude_line(&v, &mut out);
        }
    }
    Some(out)
}

/// True for weft's seeded lead/worker prompts and tool-result echoes — noise we
/// never want to show as a human turn.
fn is_seed(text: &str) -> bool {
    text.contains("weft_planner")
        || text.contains("weft_bus")
        || text.contains("You are the lead for this thread")
        || text.contains("You are a worker in weft")
        // tool-injected scaffolding (codex/opencode) — not a human turn
        || text.contains("<environment_context>")
        || text.contains("MEMORY_SUMMARY")
}

fn summarize_tool(_name: &str, input: Option<&serde_json::Value>) -> String {
    let s = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str());
    truncate(
        s("command")
            .or_else(|| s("file_path"))
            .or_else(|| s("filePath"))
            .or_else(|| s("path"))
            .or_else(|| s("pattern"))
            .unwrap_or(""),
    )
}

fn parse_claude_line(v: &serde_json::Value, out: &mut Vec<NormEvent>) {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if typ != "user" && typ != "assistant" {
        return; // system / summary / meta
    }
    // Skip subagent (sidechain) chatter — the lead has none; keep workers clean.
    if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
        return;
    }
    let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let msg = v.get("message");
    let role = msg
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or(typ)
        .to_string();
    let content = msg.and_then(|m| m.get("content"));

    let push_text = |out: &mut Vec<NormEvent>, role: &str, text: &str| {
        let t = text.trim();
        if t.is_empty() || (role == "user" && is_seed(t)) {
            return;
        }
        out.push(NormEvent::Message {
            role: role.to_string(),
            text: t.to_string(),
            ts: ts.clone(),
        });
    };

    match content {
        Some(serde_json::Value::String(s)) => push_text(out, &role, s),
        Some(serde_json::Value::Array(blocks)) => {
            let mut text = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        out.push(NormEvent::Tool {
                            name: clean_tool_name(name),
                            summary: summarize_tool(name, b.get("input")),
                            ts: ts.clone(),
                        });
                    }
                    _ => {} // tool_result and friends: skip
                }
            }
            push_text(out, &role, &text);
        }
        _ => {}
    }
}

// ---- Codex (rollout jsonl, located by session_meta cwd) ----

fn collect_jsonl(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            if let Ok(mt) = std::fs::metadata(&p).and_then(|m| m.modified()) {
                out.push((mt, p));
            }
        }
    }
}

fn read_codex(cwd: &Path) -> Option<Vec<NormEvent>> {
    let home = std::env::var("HOME").ok()?;
    let root = PathBuf::from(home).join(".codex").join("sessions");
    let canon = std::fs::canonicalize(cwd)
        .ok()
        .map(|c| c.to_string_lossy().to_string());
    let raw = cwd.to_string_lossy().to_string();

    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_jsonl(&root, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

    for (_, path) in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The first line is session_meta; match its cwd to ours.
        let first = content.lines().next().unwrap_or("");
        let meta: serde_json::Value = match serde_json::from_str(first) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mcwd = meta.pointer("/payload/cwd").and_then(|c| c.as_str()).unwrap_or("");
        if mcwd != raw && Some(mcwd.to_string()) != canon {
            continue;
        }
        let mut out = Vec::new();
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                parse_codex_line(&v, &mut out);
            }
        }
        return Some(out);
    }
    None
}

fn codex_call_summary(arguments: Option<&serde_json::Value>) -> String {
    let s = arguments.and_then(|a| a.as_str()).unwrap_or("");
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        let pick = match v.get("cmd").or_else(|| v.get("command")) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            _ => v
                .get("path")
                .or_else(|| v.get("workdir"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        };
        return truncate(&pick);
    }
    truncate(s)
}

fn parse_codex_line(v: &serde_json::Value, out: &mut Vec<NormEvent>) {
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return; // session_meta / event_msg (UI mirror) — skip
    }
    let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let Some(p) = v.get("payload") else { return };
    match p.get("type").and_then(|t| t.as_str()) {
        Some("message") => {
            let role = p.get("role").and_then(|r| r.as_str()).unwrap_or("assistant").to_string();
            let mut text = String::new();
            if let Some(arr) = p.get("content").and_then(|c| c.as_array()) {
                for b in arr {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
            }
            let text = text.trim();
            if !text.is_empty() && !(role == "user" && is_seed(text)) {
                out.push(NormEvent::Message { role, text: text.to_string(), ts });
            }
        }
        Some("function_call") => {
            let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            out.push(NormEvent::Tool {
                name: clean_tool_name(name),
                summary: codex_call_summary(p.get("arguments")),
                ts,
            });
        }
        _ => {} // function_call_output / reasoning / etc.
    }
}

// ---- OpenCode (read-only from its SQLite db, by session.directory) ----

fn parse_opencode_part(role: &str, data: &serde_json::Value, out: &mut Vec<NormEvent>) {
    match data.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            let text = data.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
            if !text.is_empty() && !(role == "user" && is_seed(text)) {
                out.push(NormEvent::Message {
                    role: role.to_string(),
                    text: text.to_string(),
                    ts: String::new(),
                });
            }
        }
        Some("tool") => {
            let name = data.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
            out.push(NormEvent::Tool {
                name: clean_tool_name(name),
                summary: summarize_tool(name, data.pointer("/state/input")),
                ts: String::new(),
            });
        }
        _ => {} // step-start / step-finish / reasoning
    }
}

async fn read_opencode(cwd: &Path) -> Option<Vec<NormEvent>> {
    use std::collections::HashMap;
    let home = std::env::var("HOME").ok()?;
    let db = PathBuf::from(home)
        .join(".local/share/opencode/opencode.db");
    if !db.exists() {
        return None;
    }
    let raw = cwd.to_string_lossy().to_string();
    let canon = std::fs::canonicalize(cwd)
        .ok()
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|| raw.clone());

    // Read-only so the live (WAL) db is never disturbed.
    let url = format!("sqlite://{}?mode=ro", db.to_string_lossy());
    let mut opt = ConnectOptions::new(url);
    opt.max_connections(1).sqlx_logging(false);
    let conn = Database::connect(opt).await.ok()?;

    let q = |sql: &str, vals: Vec<sea_orm::Value>| {
        Statement::from_sql_and_values(DbBackend::Sqlite, sql, vals)
    };

    let sid_rows = conn
        .query_all(q(
            "SELECT id FROM session WHERE directory = ? OR directory = ? ORDER BY time_updated DESC LIMIT 1",
            vec![raw.clone().into(), canon.into()],
        ))
        .await
        .ok()?;
    let session_id: String = sid_rows.first()?.try_get("", "id").ok()?;

    let mut role_of: HashMap<String, String> = HashMap::new();
    if let Ok(rows) = conn
        .query_all(q(
            "SELECT id, data FROM message WHERE session_id = ?",
            vec![session_id.clone().into()],
        ))
        .await
    {
        for r in rows {
            let id: String = r.try_get("", "id").unwrap_or_default();
            let data: String = r.try_get("", "data").unwrap_or_default();
            let role = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v.get("role").and_then(|x| x.as_str()).map(String::from))
                .unwrap_or_else(|| "assistant".into());
            role_of.insert(id, role);
        }
    }

    let mut out = Vec::new();
    let part_rows = conn
        .query_all(q(
            "SELECT message_id, data FROM part WHERE session_id = ? ORDER BY time_created, id",
            vec![session_id.into()],
        ))
        .await
        .ok()?;
    for r in part_rows {
        let mid: String = r.try_get("", "message_id").unwrap_or_default();
        let data: String = r.try_get("", "data").unwrap_or_default();
        let role = role_of.get(&mid).map(String::as_str).unwrap_or("assistant");
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            parse_opencode_part(role, &v, &mut out);
        }
    }
    let _ = conn.close().await;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_tool_calls_skips_seed() {
        let mut out = Vec::new();
        // seeded lead prompt -> skipped
        parse_claude_line(
            &serde_json::json!({"type":"user","timestamp":"t0",
                "message":{"role":"user","content":"You are the lead for this thread in weft. Use weft_planner."}}),
            &mut out,
        );
        // real human message -> kept
        parse_claude_line(
            &serde_json::json!({"type":"user","timestamp":"t1",
                "message":{"role":"user","content":"add a discount field"}}),
            &mut out,
        );
        // assistant text + a tool call
        parse_claude_line(
            &serde_json::json!({"type":"assistant","timestamp":"t2","message":{"role":"assistant","content":[
                {"type":"text","text":"On it."},
                {"type":"tool_use","name":"Bash","input":{"command":"ls -la /very/long/path/that/keeps/going"}}
            ]}}),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                NormEvent::Message { role: "user".into(), text: "add a discount field".into(), ts: "t1".into() },
                NormEvent::Tool { name: "Bash".into(), summary: "ls -la /very/long/path/that/keeps/going".into(), ts: "t2".into() },
                NormEvent::Message { role: "assistant".into(), text: "On it.".into(), ts: "t2".into() },
            ]
        );
    }

    #[test]
    fn parses_codex_messages_and_calls() {
        let mut out = Vec::new();
        // session_meta -> skipped
        parse_codex_line(
            &serde_json::json!({"type":"session_meta","payload":{"cwd":"/x"}}),
            &mut out,
        );
        // event_msg mirror -> skipped (we parse response_item only, no dupes)
        parse_codex_line(
            &serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"hi"}}),
            &mut out,
        );
        // user message
        parse_codex_line(
            &serde_json::json!({"type":"response_item","timestamp":"t1",
                "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"add a field"}]}}),
            &mut out,
        );
        // function call with arguments as a JSON string
        parse_codex_line(
            &serde_json::json!({"type":"response_item","timestamp":"t2",
                "payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"git status\"}"}}),
            &mut out,
        );
        // tool output -> skipped
        parse_codex_line(
            &serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","output":"x"}}),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                NormEvent::Message { role: "user".into(), text: "add a field".into(), ts: "t1".into() },
                NormEvent::Tool { name: "exec_command".into(), summary: "git status".into(), ts: "t2".into() },
            ]
        );
    }

    #[test]
    fn parses_opencode_parts() {
        let mut out = Vec::new();
        parse_opencode_part(
            "user",
            &serde_json::json!({"type":"text","text":"build the gift-card field"}),
            &mut out,
        );
        parse_opencode_part(
            "assistant",
            &serde_json::json!({"type":"tool","tool":"bash",
                "state":{"status":"completed","input":{"command":"npm test"}}}),
            &mut out,
        );
        parse_opencode_part(
            "assistant",
            &serde_json::json!({"type":"step-finish"}),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                NormEvent::Message { role: "user".into(), text: "build the gift-card field".into(), ts: String::new() },
                NormEvent::Tool { name: "bash".into(), summary: "npm test".into(), ts: String::new() },
            ]
        );
    }

    #[test]
    fn cleans_mcp_tool_prefixes() {
        assert_eq!(clean_tool_name("mcp__weft_bus__bus_post"), "bus_post");
        assert_eq!(clean_tool_name("weft_bus_bus_post"), "bus_post");
        assert_eq!(clean_tool_name("weft_planner_propose_directions"), "propose_directions");
        assert_eq!(clean_tool_name("Bash"), "Bash");
        assert_eq!(clean_tool_name("exec_command"), "exec_command");
    }

    #[test]
    fn skips_system_and_sidechain() {
        let mut out = Vec::new();
        parse_claude_line(&serde_json::json!({"type":"summary","summary":"x"}), &mut out);
        parse_claude_line(
            &serde_json::json!({"type":"assistant","isSidechain":true,
                "message":{"role":"assistant","content":[{"type":"text","text":"sub"}]}}),
            &mut out,
        );
        assert!(out.is_empty());
    }
}
