//! Sidecar: read a tool's own session transcript and normalize it into clean,
//! app-native events (NormEvent) for the observe-mode chat view — so the common
//! "watch the agent" case never depends on rendering a live TUI in xterm.
//!
//! Phase 2 covers Claude (the lead is always Claude): tail the session jsonl
//! under ~/.claude/projects/<encoded-cwd>/. Codex (rollout jsonl) and OpenCode
//! (SQLite / SSE) come next behind the same NormEvent shape.

use std::path::Path;

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
pub fn read_transcript(cwd: &Path, tool: &str) -> Vec<NormEvent> {
    match tool {
        "claude" => read_claude(cwd).unwrap_or_default(),
        _ => Vec::new(), // codex / opencode: later
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
        let mt = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
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
}

fn summarize_tool(_name: &str, input: Option<&serde_json::Value>) -> String {
    let s = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str());
    let pick = s("command")
        .or_else(|| s("file_path"))
        .or_else(|| s("filePath"))
        .or_else(|| s("path"))
        .or_else(|| s("pattern"))
        .unwrap_or("");
    let pick = pick.lines().next().unwrap_or("");
    if pick.chars().count() > 80 {
        pick.chars().take(80).collect::<String>() + "…"
    } else {
        pick.to_string()
    }
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
                            name: name.to_string(),
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
