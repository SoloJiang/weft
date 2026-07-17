//! Conversation rewind: fork a claude native session's transcript at a cut
//! point so the engine resumes from a copy that ends right BEFORE the chosen
//! user message. The original `<session-id>.jsonl` is never touched — it
//! stays as the natural backup. Spike-verified against claude 2.1.201.
//!
//! opencode is forked through its own HTTP API instead: a temporary
//! `opencode serve` answers `POST /session/{id}/fork` with a NEW session
//! holding every message strictly before the matched user message.
//! Spike-verified against opencode 1.17.9.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use serde_json::Value;

/// Where to cut the claude transcript.
pub enum ClaudeCut {
    /// Keep everything up to AND including the line carrying this assistant
    /// event uuid, plus any non-user/assistant turn-tail lines after it.
    AfterUuid(String),
    /// Legacy fallback for rows that predate anchor recording: cut BEFORE the
    /// `ordinal`-th (1-based) user line whose extracted text equals `text`
    /// (whitespace-normalized on both sides).
    BeforeUserText { text: String, ordinal: usize },
}

/// Fork the transcript of claude session `native_id` (cwd = the session's
/// working dir) at `cut`. Returns the NEW session id (the fork's file name),
/// or None when the cut keeps zero lines — the caller then starts a brand-new
/// native session instead of resuming.
pub fn fork_claude_at(cwd: &Path, native_id: &str, cut: &ClaudeCut) -> Result<Option<String>> {
    let dir = crate::claude::projects_dir_for(cwd)?;
    fork_transcript_file(&dir.join(format!("{native_id}.jsonl")), native_id, cut)
}

/// The file-level core of [`fork_claude_at`], split out so tests can point it
/// at a fixture without a real `~/.claude/projects` tree.
fn fork_transcript_file(src: &Path, old_id: &str, cut: &ClaudeCut) -> Result<Option<String>> {
    let raw = std::fs::read_to_string(src)
        .with_context(|| format!("read claude transcript {}", src.display()))?;
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let keep = cut_len(&lines, cut)?;
    let mut kept: Vec<&str> = lines[..keep].to_vec();
    trim_tail(&mut kept);
    if kept.is_empty() {
        return Ok(None);
    }
    let new_id = new_uuid_v4();
    let mut out = String::new();
    for line in &kept {
        out.push_str(&rewrite_line(line, old_id, &new_id));
        out.push('\n');
    }
    let dst = src.with_file_name(format!("{new_id}.jsonl"));
    std::fs::write(&dst, out).with_context(|| format!("write fork {}", dst.display()))?;
    #[cfg(unix)]
    {
        // Transcripts carry the conversation in plaintext; match claude's own
        // 0600 instead of the process umask default.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod fork {}", dst.display()))?;
    }
    Ok(Some(new_id))
}

/// How many leading lines the cut keeps (the cut itself is exclusive for
/// BeforeUserText, inclusive-of-anchor for AfterUuid).
fn cut_len(lines: &[&str], cut: &ClaudeCut) -> Result<usize> {
    match cut {
        ClaudeCut::AfterUuid(anchor) => {
            let Some(i) = lines
                .iter()
                .position(|l| line_uuid(l).as_deref() == Some(anchor.as_str()))
            else {
                return Err(anyhow!(
                    "anchor assistant uuid not found in the transcript"
                ));
            };
            // Keep the anchor line, then any turn-tail lines that are neither
            // user nor assistant (attachments etc.), stopping before the next
            // turn's first user/assistant line.
            let mut end = i + 1;
            while end < lines.len() {
                match line_kind(lines[end]) {
                    LineKind::User | LineKind::Assistant => break,
                    LineKind::Other => end += 1,
                }
            }
            Ok(end)
        }
        ClaudeCut::BeforeUserText { text, ordinal } => {
            if *ordinal == 0 {
                return Err(anyhow!("ordinal is 1-based"));
            }
            let want = normalize_ws(text);
            let mut seen = 0usize;
            for (i, line) in lines.iter().enumerate() {
                let Some(t) = user_text(line) else { continue };
                if normalize_ws(&t) == want {
                    seen += 1;
                    if seen == *ordinal {
                        return Ok(i);
                    }
                }
            }
            Err(anyhow!(
                "user message text not found in the transcript (match #{ordinal})"
            ))
        }
    }
}

/// Shape the kept tail so a `--resume` of the fork doesn't trip on an
/// interrupted turn: repeatedly strip trailing control lines
/// (queue-operation / last-prompt / mode), then drop a trailing assistant
/// line whose tool_use has no tool_result after it (being last, it can never
/// get one — claude refuses to resume a dangling tool_use).
fn trim_tail(kept: &mut Vec<&str>) {
    loop {
        while kept.last().is_some_and(|l| is_control_line(l)) {
            kept.pop();
        }
        let Some(last) = kept.last() else { return };
        if assistant_with_tool_use(last) {
            kept.pop();
        } else {
            return;
        }
    }
}

/// The only line classification the cut logic needs.
enum LineKind {
    User,
    Assistant,
    Other,
}

fn line_kind(line: &str) -> LineKind {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return LineKind::Other;
    };
    match v["type"].as_str() {
        Some("user") => LineKind::User,
        Some("assistant") => LineKind::Assistant,
        _ => LineKind::Other,
    }
}

fn line_uuid(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line).ok()?["uuid"]
        .as_str()
        .map(String::from)
}

/// A transcript user line's visible text: the string content as-is, or the
/// concatenation of its `text` blocks (tool_result payloads are NOT text).
fn user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"].as_str() != Some("user") {
        return None;
    }
    let content = &v["message"]["content"];
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let blocks = content.as_array()?;
    let mut text = String::new();
    for b in blocks {
        if b["type"].as_str() == Some("text") {
            if let Some(t) = b["text"].as_str() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Whitespace-insensitive text identity, shared by every cut/match path so a
/// DB-side ordinal and a transcript-side match can never disagree (a `hello
/// world` vs `hello  world` pair once produced exactly that fork/truncate
/// mismatch).
pub(crate) fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 1-based position of `target` among `texts` under [`normalize_ws`] identity.
/// The engine computes a fallback cut ordinal from DB rows with this, so it
/// matches the transcript-side normalized matching exactly. 0 = no match.
pub(crate) fn ordinal_of(texts: &[String], target: &str) -> usize {
    let want = normalize_ws(target);
    if want.is_empty() {
        return 0;
    }
    texts.iter().filter(|t| normalize_ws(t) == want).count()
}

fn is_control_line(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    matches!(
        v["type"].as_str(),
        Some("queue-operation") | Some("last-prompt") | Some("mode")
    )
}

fn assistant_with_tool_use(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if v["type"].as_str() != Some("assistant") {
        return false;
    }
    v["message"]["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|b| b["type"].as_str() == Some("tool_use"))
    })
}

/// Rewrite one kept line for the fork: structurally set `sessionId` to the
/// new id, then run a BYTE-level old→new replace over the serialization —
/// hook outputs embed the session id escaped inside strings (spike), which
/// the structured pass alone misses.
fn rewrite_line(line: &str, old_id: &str, new_id: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(line) else {
        return line.replace(old_id, new_id);
    };
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.insert("sessionId".to_string(), Value::String(new_id.to_string()));
        }
    }
    match serde_json::to_string(&v) {
        Ok(s) => s.replace(old_id, new_id),
        Err(_) => line.replace(old_id, new_id),
    }
}

/// claude session ids are RFC 4122 uuids; mint a v4 (no uuid crate in-tree).
fn new_uuid_v4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ───────────────────────────── opencode ─────────────────────────────

/// Hard kill guard for the temporary serve: every exit path out of
/// [`fork_opencode_at`] (early `?` returns included) drops this, and
/// `start_kill` (sync, no runtime needed) plus tokio's `kill_on_drop` leave
/// no orphan serve process behind.
struct KillOnDrop(tokio::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

/// Fork opencode session `session_id` (cwd = the session's working dir) right
/// BEFORE the `ordinal`-th (1-based) user message whose text matches `text`.
/// Returns the NEW session id. opencode has fork-at-point built in
/// (1.17.9 spike): `POST /session/{id}/fork?directory=<cwd>` with
/// `{"messageID": "<id>"}` creates a new session holding every message
/// STRICTLY BEFORE that message id (fresh message ids; the original is
/// untouched) — exactly Weft's cut-before semantics.
///
/// `opencode run` is per-turn, so its ephemeral server is gone by rewind
/// time; a temporary `opencode serve` is spawned for the two calls and
/// killed on every exit path. The caller handles the no-native-id and
/// rewind-to-before-the-first-message cases (fresh session, no fork).
pub async fn fork_opencode_at(
    command: &str,
    cwd: &Path,
    session_id: &str,
    text: &str,
    ordinal: usize,
) -> Result<String> {
    if ordinal == 0 {
        return Err(anyhow!("ordinal is 1-based"));
    }
    let child = tokio::process::Command::new(command)
        .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        .current_dir(cwd)
        // Same PATH augmentation as the engine's `opencode run` spawn, and the
        // same password removal as the discovery serve (we never set one).
        .env("PATH", crate::detect::tool_path())
        .env_remove("OPENCODE_SERVER_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {command} serve"))?;
    let mut guard = KillOnDrop(child);
    let base = serve_listen_url(&mut guard.0).await?;

    let client = reqwest::Client::new();
    let cwd_s = cwd.to_string_lossy().into_owned();
    let messages: Vec<Value> = client
        .get(format!("{base}/session/{session_id}/message"))
        .query(&[("directory", cwd_s.as_str())])
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(cut) = match_user_cut(&messages, text, ordinal) else {
        return Err(anyhow!("在该会话历史中找不到目标消息"));
    };

    // Response shape (1.17.9, verbatim capture): the NEW session object with
    // the id at top level —
    // {"id":"ses_091fe8514ffe5ok8uX5ikGR69R","slug":"happy-orchid",
    //  "projectID":"global","directory":"/private/tmp/weft-oc-spike",
    //  "title":"New session - 2026-07-17T02:54:20.240Z (fork #1)",
    //  "version":"1.17.9","time":{…},…}
    let fork: Value = client
        .post(format!("{base}/session/{session_id}/fork"))
        .query(&[("directory", cwd_s.as_str())])
        .json(&serde_json::json!({ "messageID": cut }))
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let new_id = fork["id"]
        .as_str()
        .ok_or_else(|| anyhow!("opencode fork response missing the new session id"))?
        .to_string();
    drop(guard); // explicit: KillOnDrop::drop start_kills the serve
    Ok(new_id)
}

/// The first `http://…` URL the serve prints (stdout or stderr), with the
/// same 15s startup cap the discovery serve uses.
async fn serve_listen_url(child: &mut tokio::process::Child) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);
    crate::opencode::drain_for_url(child.stdout.take(), tx.clone());
    crate::opencode::drain_for_url(child.stderr.take(), tx);
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .map_err(|_| anyhow!("opencode serve did not report a listening URL in 15s"))?
        .ok_or_else(|| anyhow!("opencode serve exited before listening"))
}

/// The id of the user message to cut before: the `ordinal`-th (1-based) user
/// message whose text matches `text`, whitespace-normalized on both sides
/// (same normalization as claude's BeforeUserText cut). `opencode run` wraps
/// the positional message in literal double quotes when storing it (1.17.9
/// spike), so the quote-stripped form counts as equal too. Exact identity
/// only — no suffix/prefix tolerance, which can select the wrong message.
fn match_user_cut(messages: &[Value], text: &str, ordinal: usize) -> Option<String> {
    if ordinal == 0 {
        return None;
    }
    let want = normalize_ws(text);
    if want.is_empty() {
        return None;
    }
    let mut seen = 0usize;
    for m in messages {
        if m["info"]["role"].as_str() != Some("user") {
            continue;
        }
        let stored = normalize_ws(&message_text(m));
        if stored.is_empty() {
            continue;
        }
        // Exact identity only. Weft sends the opencode message verbatim (no
        // system-prompt prefixing in the argv), so a looser rule (e.g. an
        // ends-with tolerance) can mistake an earlier message that merely
        // ENDS WITH the target text for the target itself — forking at the
        // wrong point while the timeline truncates at the right one.
        if stored == want || strip_outer_quotes(&stored) == want {
            seen += 1;
            if seen == ordinal {
                return m["info"]["id"].as_str().map(String::from);
            }
        }
    }
    None
}

/// A message's visible text: its `text` parts concatenated (file/step parts
/// carry nothing the user typed).
fn message_text(m: &Value) -> String {
    let mut text = String::new();
    let Some(parts) = m["parts"].as_array() else {
        return text;
    };
    for p in parts {
        if p["type"].as_str() == Some("text") {
            if let Some(t) = p["text"].as_str() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    text
}

/// Strip ONE layer of surrounding double quotes (opencode run's stored form).
fn strip_outer_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_line(uuid: &str, sid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": sid,
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    fn assistant_line(uuid: &str, sid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "sessionId": sid,
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    fn tool_use_line(uuid: &str, sid: &str, tool_id: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "sessionId": sid,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": tool_id, "name": "Bash", "input": {}}
            ]},
        })
        .to_string()
    }

    fn tool_result_line(uuid: &str, sid: &str, tool_id: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": sid,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}
            ]},
        })
        .to_string()
    }

    /// Write `lines` to a fresh `<tmp>/<old_id>.jsonl`; returns (dir, src).
    fn fixture(old_id: &str, lines: &[String]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join(format!("{old_id}.jsonl"));
        std::fs::write(&src, lines.join("\n") + "\n").expect("write fixture");
        (dir, src)
    }

    fn read_fork(dir: &Path, new_id: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{new_id}.jsonl"))).expect("read fork")
    }

    #[test]
    fn after_uuid_keeps_turn_tail_and_strips_control_lines() {
        let lines = vec![
            user_line("u1", "sid-old", "first"),
            assistant_line("a1", "sid-old", "answer one"),
            // turn-tail attachment line (neither user nor assistant) is kept
            "{\"type\":\"attachment\",\"uuid\":\"att1\",\"sessionId\":\"sid-old\"}".to_string(),
            // control lines riding the turn tail are stripped off the end
            "{\"type\":\"queue-operation\",\"sessionId\":\"sid-old\"}".to_string(),
            user_line("u2", "sid-old", "second"),
            assistant_line("a2", "sid-old", "answer two"),
        ];
        let (dir, src) = fixture("sid-old", &lines);
        let before = std::fs::read_to_string(&src).expect("read src");

        let new_id = fork_transcript_file(&src, "sid-old", &ClaudeCut::AfterUuid("a1".into()))
            .expect("fork")
            .expect("some");
        let out = read_fork(dir.path(), &new_id);
        assert!(out.contains("\"uuid\":\"a1\""), "anchor line kept: {out}");
        assert!(out.contains("\"type\":\"attachment\""), "turn tail kept: {out}");
        assert!(!out.contains("queue-operation"), "control line stripped: {out}");
        assert!(!out.contains("\"uuid\":\"u2\""), "next turn cut: {out}");
        // Original untouched.
        assert_eq!(std::fs::read_to_string(&src).expect("read src"), before);
    }

    #[test]
    fn after_uuid_missing_anchor_errors() {
        let lines = vec![user_line("u1", "sid-old", "first")];
        let (_dir, src) = fixture("sid-old", &lines);
        let r = fork_transcript_file(&src, "sid-old", &ClaudeCut::AfterUuid("nope".into()));
        assert!(r.is_err(), "missing anchor must error");
    }

    #[test]
    fn before_user_text_cuts_at_ordinal_match() {
        let lines = vec![
            user_line("u1", "sid-old", "same text"),
            assistant_line("a1", "sid-old", "one"),
            user_line("u2", "sid-old", "same text"),
            assistant_line("a2", "sid-old", "two"),
        ];
        let (dir, src) = fixture("sid-old", &lines);
        // ordinal 2 = the SECOND user line with that text → keep through a1.
        let new_id = fork_transcript_file(
            &src,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "same text".into(), ordinal: 2 },
        )
        .expect("fork")
        .expect("some");
        let out = read_fork(dir.path(), &new_id);
        assert!(out.contains("\"uuid\":\"u1\""), "first match kept: {out}");
        assert!(out.contains("\"uuid\":\"a1\""), "turn one kept: {out}");
        assert!(!out.contains("\"uuid\":\"u2\""), "second match cut: {out}");

        // Whitespace normalization: extra spaces/newlines still match.
        let lines2 = vec![
            user_line("u1", "sid-old", "hello   world\nagain"),
            assistant_line("a1", "sid-old", "one"),
        ];
        let (dir2, src2) = fixture("sid-old", &lines2);
        let r = fork_transcript_file(
            &src2,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "hello world again".into(), ordinal: 1 },
        )
        .expect("fork");
        assert!(r.is_none(), "normalized match cuts everything before it: {r:?}");
        let _ = dir2;

        // No match → Err.
        let r = fork_transcript_file(
            &src,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "same text".into(), ordinal: 3 },
        );
        assert!(r.is_err(), "ordinal past the last match must error");
    }

    #[test]
    fn dangling_tool_use_tail_is_trimmed() {
        let lines = vec![
            user_line("u1", "sid-old", "go"),
            assistant_line("a1", "sid-old", "working"),
            tool_use_line("a2", "sid-old", "toolu_1"),
            // interrupted here: no tool_result ever arrived
            user_line("u2", "sid-old", "next question"),
        ];
        let (dir, src) = fixture("sid-old", &lines);
        // Cut before u2 → kept tail is the dangling tool_use line → trimmed.
        let new_id = fork_transcript_file(
            &src,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "next question".into(), ordinal: 1 },
        )
        .expect("fork")
        .expect("some");
        let out = read_fork(dir.path(), &new_id);
        assert!(!out.contains("toolu_1"), "dangling tool_use dropped: {out}");
        assert!(out.contains("\"uuid\":\"a1\""), "text answer survives: {out}");

        // A COMPLETED tool pair at the tail stays (result line last is fine).
        let lines2 = vec![
            user_line("u1", "sid-old", "go"),
            tool_use_line("a1", "sid-old", "toolu_1"),
            tool_result_line("u2", "sid-old", "toolu_1"),
            user_line("u3", "sid-old", "next"),
        ];
        let (dir2, src2) = fixture("sid-old", &lines2);
        let new_id2 = fork_transcript_file(
            &src2,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "next".into(), ordinal: 1 },
        )
        .expect("fork")
        .expect("some");
        let out2 = read_fork(dir2.path(), &new_id2);
        assert!(out2.contains("toolu_1"), "completed tool pair kept: {out2}");
    }

    #[test]
    fn escaped_session_id_is_rewritten_byte_level() {
        // The old id appears as the sessionId field AND escaped inside a hook
        // output string — the structured pass alone misses the second one.
        let hook = format!(
            "{{\"type\":\"system\",\"sessionId\":\"sid-old\",\"hook\":\"ran in \\\"sid-old\\\" ok\"}}"
        );
        let lines = vec![user_line("u1", "sid-old", "first"), hook];
        let (dir, src) = fixture("sid-old", &lines);
        let new_id = fork_transcript_file(&src, "sid-old", &ClaudeCut::AfterUuid("u1".into()))
            .expect("fork")
            .expect("some");
        let out = read_fork(dir.path(), &new_id);
        assert!(!out.contains("sid-old"), "no old id anywhere: {out}");
        assert!(out.contains(&new_id), "new id present: {out}");
        // Every kept line parses and carries the new sessionId.
        for l in out.lines() {
            let v: Value = serde_json::from_str(l).expect("line parses");
            assert_eq!(v["sessionId"].as_str(), Some(new_id.as_str()));
        }
    }

    #[test]
    fn empty_cut_returns_none_and_writes_nothing() {
        let lines = vec![
            user_line("u1", "sid-old", "first ever"),
            assistant_line("a1", "sid-old", "hi"),
        ];
        let (dir, src) = fixture("sid-old", &lines);
        let r = fork_transcript_file(
            &src,
            "sid-old",
            &ClaudeCut::BeforeUserText { text: "first ever".into(), ordinal: 1 },
        )
        .expect("fork");
        assert_eq!(r, None, "cut before the first line keeps nothing");
        // No fork file was written next to the source.
        let n = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .count();
        assert_eq!(n, 1, "only the original file remains");
    }

    #[cfg(unix)]
    #[test]
    fn fork_file_mode_is_600() {
        use std::os::unix::fs::PermissionsExt;
        let lines = vec![
            user_line("u1", "sid-old", "first"),
            assistant_line("a1", "sid-old", "hi"),
        ];
        let (dir, src) = fixture("sid-old", &lines);
        let new_id = fork_transcript_file(&src, "sid-old", &ClaudeCut::AfterUuid("a1".into()))
            .expect("fork")
            .expect("some");
        let mode = std::fs::metadata(dir.path().join(format!("{new_id}.jsonl")))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "fork must be owner-only");
    }

    #[test]
    fn new_uuid_v4_shape() {
        let id = new_uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(id.as_bytes()[14], b'4', "version nibble");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant nibble"
        );
    }

    // ── opencode fork ──

    fn oc_user(id: &str, texts: &[&str]) -> Value {
        let parts: Vec<Value> = texts
            .iter()
            .map(|t| serde_json::json!({"type": "text", "text": t}))
            .collect();
        serde_json::json!({"info": {"id": id, "role": "user"}, "parts": parts})
    }

    fn oc_assistant(id: &str, text: &str) -> Value {
        serde_json::json!({
            "info": {"id": id, "role": "assistant"},
            "parts": [
                {"type": "step-start"},
                {"type": "text", "text": text},
                {"type": "step-finish"},
            ],
        })
    }

    /// Real opencode 1.17.9 serve startup output, captured on this machine
    /// (`opencode serve --hostname 127.0.0.1 --port 0`, both lines on stdout).
    #[test]
    fn serve_listen_line_yields_its_url() {
        let warning = "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.";
        let listening = "opencode server listening on http://127.0.0.1:4096";
        assert_eq!(crate::opencode::parse_url(warning), None);
        assert_eq!(
            crate::opencode::parse_url(listening).as_deref(),
            Some("http://127.0.0.1:4096")
        );
    }

    #[test]
    fn opencode_cut_picks_the_ordinal_th_duplicate() {
        let msgs = vec![
            oc_user("m1", &["same text"]),
            oc_assistant("a1", "one"),
            oc_user("m2", &["same text"]),
            oc_assistant("a2", "two"),
            oc_user("m3", &["same text"]),
        ];
        assert_eq!(match_user_cut(&msgs, "same text", 1).as_deref(), Some("m1"));
        assert_eq!(match_user_cut(&msgs, "same text", 2).as_deref(), Some("m2"));
        assert_eq!(match_user_cut(&msgs, "same text", 3).as_deref(), Some("m3"));
        assert_eq!(match_user_cut(&msgs, "same text", 4), None, "ordinal past the end");
        assert_eq!(match_user_cut(&msgs, "same text", 0), None, "ordinal is 1-based");
    }

    #[test]
    fn opencode_cut_normalizes_whitespace() {
        // Real capture: inner spacing/newlines survive verbatim into storage.
        let msgs = vec![oc_user("m1", &["\"say \"hi\"   there\nnext line\""])];
        assert_eq!(
            match_user_cut(&msgs, "say \"hi\" there next line", 1).as_deref(),
            Some("m1")
        );
    }

    #[test]
    fn opencode_cut_unwraps_runs_quote_layer() {
        // 1.17.9 spike: `opencode run <msg>` stores the message wrapped in
        // literal double quotes; the text Weft sent (and the timeline holds)
        // has none.
        let msgs = vec![
            oc_user("m1", &["\"Reply with exactly the word ALPHA and nothing else.\""]),
            oc_assistant("a1", "ALPHA"),
            oc_user("m2", &["\"Reply with exactly the word BRAVO and nothing else.\""]),
        ];
        assert_eq!(
            match_user_cut(&msgs, "Reply with exactly the word BRAVO and nothing else.", 1)
                .as_deref(),
            Some("m2")
        );
        // Text parts concatenate; non-text parts are ignored.
        let msgs2 = vec![oc_user("m9", &["\"multi", "part\""])];
        assert_eq!(match_user_cut(&msgs2, "multi part", 1).as_deref(), Some("m9"));
    }

    #[test]
    fn opencode_cut_never_matches_suffixes() {
        // Codex-review regression: a stored first message that merely ENDS
        // WITH the target text must NOT be selected over the message that IS
        // the target — the fork would cut before the first message while the
        // timeline truncates at the later one (divergent histories).
        let msgs = vec![
            oc_user("m1", &["\"please implement hello\""]),
            oc_assistant("a1", "done"),
            oc_user("m2", &["\"hello\""]),
        ];
        assert_eq!(match_user_cut(&msgs, "hello", 1).as_deref(), Some("m2"));
        // And without an exact candidate it is a plain no-match (no tolerated
        // prefix path to fall into), even on the first user message.
        let msgs2 = vec![
            oc_user("m1", &["\"please implement hello\""]),
            oc_assistant("a1", "done"),
        ];
        assert_eq!(match_user_cut(&msgs2, "hello", 1), None);
    }

    #[test]
    fn opencode_cut_no_match_is_none() {
        let msgs = vec![oc_user("m1", &["\"something\""]), oc_assistant("a1", "ok")];
        assert_eq!(match_user_cut(&msgs, "absent", 1), None);
        assert_eq!(match_user_cut(&msgs, "", 1), None, "empty target never matches");
        // Assistant turns are never candidates, even with the same text.
        let msgs2 = vec![oc_assistant("a1", "user-looking text")];
        assert_eq!(match_user_cut(&msgs2, "user-looking text", 1), None);
    }

    #[test]
    fn strip_outer_quotes_one_layer_only() {
        assert_eq!(strip_outer_quotes("\"hello\""), "hello");
        assert_eq!(strip_outer_quotes("\"a\"b\""), "a\"b");
        assert_eq!(strip_outer_quotes("hello"), "hello");
        assert_eq!(strip_outer_quotes("\""), "\"", "a lone quote is not a layer");
        assert_eq!(strip_outer_quotes("\"\""), "");
    }

    #[test]
    fn ordinal_of_counts_by_normalized_identity() {
        // Codex-review regression: DB-side ordinal must count `hello  world`
        // and `hello world` as the same message, or the transcript cut (which
        // normalizes) picks a different line than the timeline truncation.
        let texts = vec![
            "hello  world".to_string(), // normalized-equal to the target
            "unrelated".to_string(),
            "hello world".to_string(), // the target itself
        ];
        assert_eq!(ordinal_of(&texts, "hello world"), 2);
        assert_eq!(ordinal_of(&texts, "absent"), 0);
        assert_eq!(ordinal_of(&texts, ""), 0);
    }
}
