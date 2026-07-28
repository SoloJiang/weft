//! Conversation rewind: fork a claude native session's transcript at a cut
//! point so the engine resumes from a copy that ends right BEFORE the chosen
//! user message. The original `<session-id>.jsonl` is never touched — it
//! stays as the natural backup. Spike-verified against claude 2.1.201.
//!
//! opencode is forked through its own HTTP API instead: a temporary
//! `opencode serve` answers `POST /session/{id}/fork` with a NEW session
//! holding every message strictly before the matched user message.
//! Spike-verified against opencode 1.17.9.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use serde_json::Value;

/// Whether a user row actually reached the native session (and so is part of
/// the history a fork can match): delivered turns (`complete`, or
/// `interrupted` mid-turn) count; `queued` (never sent) and `error` rows
/// (synthetic failure paths like agent-not-found) never entered the native
/// history and must not skew the text-match ordinal or the anchor choice.
pub(crate) fn native_delivered(role: &str, status: &str) -> bool {
    role == "user" && matches!(status, "complete" | "interrupted")
}

/// Rebuild the EXACT text the engine dispatched to the agent for a user row
/// (the transcript stores this, not the raw input): `send` appends attachment
/// instructions and spilled-image paths to the prompt, so a native text match
/// built from the raw `content.text` can never hit for attachment-bearing
/// messages. Mirrors the construction in engine's send() — keep in sync with
/// it. Images spill under `<tmp>/weft-attachments/msg<row_id>-<i>.<ext>`;
/// send() only appends a path when the spill write succeeded, so a failed
/// spill degrades to the same honest no-match as before.
///
/// `per_turn_tool` selects the dialect path: per-turn dialects (codex exec,
/// opencode) get the image-spill appendix; resident claude gets images inline
/// (no appendix in the text block).
pub(crate) fn dispatched_text(per_turn_tool: bool, row_id: i32, content_json: &str) -> String {
    let v: Value = serde_json::from_str(content_json).unwrap_or_default();
    // The exact prompt send() persisted for image-bearing rows (a failed
    // decode/write omits that path, which no reconstruction can know).
    if let Some(d) = v["dispatched"].as_str() {
        return d.to_string();
    }
    let mut out = v["text"].as_str().unwrap_or_default().to_string();
    let list = |key: &str| -> Vec<String> {
        v[key]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let files = list("files");
    if !files.is_empty() {
        out.push_str("\n\nAttached files (read them as needed):\n");
        for f in &files {
            out.push_str(&format!("- {f}\n"));
        }
    }
    let images = list("images");
    if per_turn_tool && !images.is_empty() {
        let dir = std::env::temp_dir().join("weft-attachments");
        out.push_str("\n\nAttached images (read them as needed):\n");
        for (i, uri) in images.iter().enumerate() {
            let mt = uri
                .strip_prefix("data:")
                .and_then(|r| r.split(';').next())
                .unwrap_or_default();
            let ext = mt.rsplit('/').next().unwrap_or("png");
            out.push_str(&format!("- {}\n", dir.join(format!("msg{row_id}-{i}.{ext}")).display()));
        }
    }
    out
}

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
///
/// An EMPTY target is "no match" here because that is claude's rule, not a
/// universal one: [`user_text`] skips a transcript user line carrying no text
/// blocks, so an empty target is genuinely unaddressable in that dialect and
/// counting it would only trade a clear "no anchor" error for a confusing
/// "text not found" one. Dialects that DO record an empty prompt verbatim use
/// [`ordinal_of_prompt`].
pub(crate) fn ordinal_of(texts: &[String], target: &str) -> usize {
    if normalize_ws(target).is_empty() {
        return 0;
    }
    ordinal_of_prompt(texts, target)
}

/// [`ordinal_of`] for dialects where an empty prompt is still an addressable
/// message — ACP in particular.
///
/// `session/prompt` always sends a text block, `{"type":"text","text":""}`
/// even for an image-only message (images ride as sibling image blocks rather
/// than the spilled-path appendix per-turn tools get). So "" is a real entry
/// in the transcript, and `fork_omp_at`'s matcher already treats it as one —
/// but the engine's `ordinal == 0` guard rejected it upstream, so rewinding a
/// non-first image-only message failed as "no rewind anchor" and the matcher
/// never ran.
pub(crate) fn ordinal_of_prompt(texts: &[String], target: &str) -> usize {
    let want = normalize_ws(target);
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
/// new id, then rewrite only the EMBEDDED PROTOCOL SHAPES the structured pass
/// can't reach (spike: hook payloads carry the id escaped inside strings).
/// A bare uuid in user/assistant/tool text is CONTENT, not protocol — a
/// global byte replace would let the fork disagree with the Weft timeline
/// (codex review), so it is deliberately left untouched.
fn rewrite_line(line: &str, old_id: &str, new_id: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(line) else {
        // Not JSON (shouldn't exist in a transcript) — nothing safe to rewrite.
        return line.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.insert("sessionId".to_string(), Value::String(new_id.to_string()));
        }
    }
    match serde_json::to_string(&v) {
        Ok(s) => rewrite_protocol_embeds(&s, old_id, new_id),
        Err(_) => line.to_string(),
    }
}

/// Embedded protocol fields carrying the session id inside STRINGS: hook
/// payloads embed it as escaped snake_case JSON (`\"session_id\":\"<id>\"`)
/// and as the transcript path (`<id>.jsonl`). Only these exact wrappers are
/// rewritten; the same uuid anywhere else (e.g. pasted into a chat message)
/// is content and stays verbatim.
fn rewrite_protocol_embeds(s: &str, old_id: &str, new_id: &str) -> String {
    let escaped = format!("\\\"session_id\\\":\\\"{old_id}\\\"");
    let path_form = format!("{old_id}.jsonl");
    s.replace(&escaped, &format!("\\\"session_id\\\":\\\"{new_id}\\\""))
        .replace(&path_form, &format!("{new_id}.jsonl"))
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
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        .current_dir(cwd)
        // Same PATH augmentation as the engine's `opencode run` spawn, and the
        // same password removal as the discovery serve (we never set one).
        .env("PATH", crate::detect::tool_path())
        .env_remove("OPENCODE_SERVER_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // T1: own process group + marker before spawn; register alongside the serve
    // guard (reg lives until fork_opencode_at returns). Reclaim is T2's.
    let configured =
        crate::proc_registry::configure(&mut cmd, crate::proc_registry::Owner::preview(session_id));
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {command} serve"))?;
    let _reg = configured.register(&child);
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
        // wrong point while the timeline truncates at the right one. Both
        // forms are compared whitespace-normalized (the quote strip can leave
        // an edge space when text ends just before the closing quote).
        if stored == want || normalize_ws(strip_outer_quotes(&stored)) == want {
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




/// Whether an omp jsonl user-message body matches the rewind target.
///
/// Exact whitespace-normalized equality always matches. The FIRST user message
/// may also match after stripping the `{system}\n\n{user}` prepend Weft adds on
/// session start — `system` is passed in, not guessed.
///
/// It used to be guessed, as "everything before the LAST blank line". A
/// multi-paragraph first prompt breaks that in both directions: `SYS\n\nHello\n\
/// \nWorld` isolates only `World`, so a later message whose text is `World`
/// falsely matches the first turn and steals its ordinal (rewind then cuts the
/// whole session away), while the real target `Hello\n\nWorld` fails to match
/// its own first occurrence. An attachment appendix — which `dispatched_text`
/// appends as its own blank-line block — puts every attachment-bearing first
/// prompt in that same shape.
///
/// A mismatched `system` (the prompt changed since the session opened) simply
/// declines the relaxed match; the caller then reports no anchor rather than
/// cutting at the wrong message.
fn omp_user_body_matches(
    body: &str,
    want_norm: &str,
    first_user_prepend: Option<&str>,
) -> bool {
    if normalize_ws(body) == want_norm {
        return true;
    }
    // Only the first user turn carries the prepend.
    let Some(system) = first_user_prepend else {
        return false;
    };
    if system.is_empty() {
        return false;
    }
    let Some(rest) = body
        .strip_prefix(system)
        .and_then(|rest| rest.strip_prefix("\n\n"))
    else {
        return false;
    };
    normalize_ws(rest) == want_norm
}

/// Line index of the `ordinal`-th user prompt whose text blocks match `text`,
/// i.e. where a cut-before rewind should truncate. `None` = no such prompt.
///
/// Split from [`fork_omp_at`]'s file IO so the matching rule — including the
/// empty-text (image-only) prompt that [`ordinal_of_prompt`] can now address —
/// is testable end to end without a session file on disk.
fn omp_cut_index(
    lines: &[&str],
    text: &str,
    ordinal: usize,
    system_prompt: &str,
) -> Option<usize> {
    let want = normalize_ws(text);
    let mut user_hits = 0usize;
    let mut seen_users = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let role = v
            .pointer("/message/role")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if role != "user" {
            continue;
        }
        let content = v.pointer("/message/content").and_then(|c| c.as_array());
        // Tool-result rows are also role=user but have no direct text block.
        // Attachment-only prompts DO have a text block (text may be empty).
        let Some(arr) = content else { continue };
        let has_text_block = arr.iter().any(|b| {
            b.get("type").and_then(|t| t.as_str()) == Some("text")
                || b.get("text").and_then(|t| t.as_str()).is_some()
        });
        if !has_text_block {
            continue;
        }
        let body = arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        let first_user_prepend = (seen_users == 0).then_some(system_prompt);
        seen_users += 1;
        if !omp_user_body_matches(&body, &want, first_user_prepend) {
            continue;
        }
        user_hits += 1;
        if user_hits == ordinal {
            return Some(i);
        }
    }
    None
}

/// Cut-before rewind for omp ACP sessions.
///
/// ACP `session/fork` only does full-history copy. We rewrite the on-disk
/// `~/.omp/agent/sessions/<encoded-cwd>/*_<id>.jsonl` to keep entries strictly
/// before the Nth matching user message, mint a new session id, and return it
/// so the engine can `session/load` next turn. Spike-verified: hand-cut files
/// load and only see the kept prefix (omp 17.1.1).
pub fn fork_omp_at(
    cwd: &Path,
    session_id: &str,
    text: &str,
    ordinal: usize,
    system_prompt: &str,
) -> Result<Option<String>> {
    if ordinal == 0 {
        return Err(anyhow!("ordinal is 1-based"));
    }
    let Some(src) = find_omp_session_file(cwd, session_id)? else {
        return Err(anyhow!("omp_session_not_found"));
    };
    let raw = std::fs::read_to_string(&src)
        .with_context(|| format!("read omp session {}", src.display()))?;
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let Some(cut) = omp_cut_index(&lines, text, ordinal, system_prompt) else {
        return Err(anyhow!("omp_user_not_found"));
    };
    if cut == 0 {
        // Nothing to keep before first line — fresh session.
        return Ok(None);
    }
    let kept = &lines[..cut];
    if kept.iter().all(|l| {
        serde_json::from_str::<Value>(l)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .map(|t| t != "message")
            .unwrap_or(true)
    }) {
        // Only headers — equivalent to before-first-message.
        return Ok(None);
    }
    // Random, not clock-derived: two rewinds on the same tick produced the SAME
    // id, and both write `weft-rewind_<id>.jsonl` with a truncating `fs::write`,
    // so one fork silently overwrote the other and two sessions resumed the same
    // (or a corrupted) history. `new_uuid_v4` is the same RFC 4122 v4 helper the
    // claude rewind already uses.
    let new_id = new_uuid_v4();
    let mut out = String::new();
    for line in kept {
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session") {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("id".into(), Value::String(new_id.clone()));
                obj.remove("parentSession");
            }
        }
        out.push_str(&v.to_string());
        out.push('\n');
    }
    let parent = src.parent().unwrap_or(Path::new("."));
    let dst = parent.join(format!("weft-rewind_{new_id}.jsonl"));
    std::fs::write(&dst, out).with_context(|| format!("write omp fork {}", dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod omp fork {}", dst.display()))?;
    }
    Ok(Some(new_id))
}

fn find_omp_session_file(cwd: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let root = home.join(".omp/agent/sessions");
    // The ROOT itself must not be a symlink. `is_dir()` follows one, and the
    // bucket/entry checks below only reject a symlink at the FINAL component —
    // so a redirected root put the whole scan outside the intended tree, and
    // rewind then writes its fork beside that external file.
    let root_ok = std::fs::symlink_metadata(&root)
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false);
    if !root_ok {
        return Ok(None);
    }
    // Containment of REAL locations, checked once at the end. Rejecting a
    // symlink component-by-component can only cover the components this code
    // happens to look at; canonicalizing the winner and requiring it under the
    // canonical root covers every intermediate component and any `..` escape.
    // Same rule `bus::builtin_allow` uses for ask-bridge path containment.
    let canon_root = std::fs::canonicalize(&root)?;
    // Prefer encoded-cwd bucket; fall back to any match.
    let encoded = omp_encode_cwd(cwd);
    let mut candidates = Vec::new();
    // Whether a scan stopped at its cap. "Nothing found" and "stopped looking"
    // are different answers, and reporting the second as the first told the
    // user their session was gone when it was merely past the cap.
    let mut truncated = false;
    let preferred = root.join(&encoded);
    // Do not follow a symlink bucket (or entries): rewind must stay under
    // ~/.omp/agent/sessions.
    let preferred_ok = std::fs::symlink_metadata(&preferred)
        .map(|m| m.is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false);
    if preferred_ok {
        const MAX_PREFERRED_SCAN: usize = 4_096;
        const MAX_PREFERRED_HITS: usize = 32;
        let mut scanned = 0usize;
        for e in std::fs::read_dir(&preferred)? {
            if scanned >= MAX_PREFERRED_SCAN {
                truncated = true;
                break;
            }
            let e = e?;
            scanned += 1;
            let p = e.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(session_id) && name.ends_with(".jsonl") {
                candidates.push(p);
                if candidates.len() >= MAX_PREFERRED_HITS {
                    break;
                }
            }
        }
    }
    if candidates.is_empty() {
        let (hits, walk_truncated) = walkdir_jsonl(&root, session_id)?;
        truncated = truncated || walk_truncated;
        candidates.extend(hits);
    }
    candidates.retain(|p| is_inside_canonical(p, &canon_root));
    // Distinguish the two failure answers. `Ok(None)` means "scanned everything
    // reachable, it is not there"; a truncated scan cannot claim that, and
    // `fork_omp_at` would otherwise report `omp_session_not_found` for a
    // session that exists just past the cap.
    if candidates.is_empty() && truncated {
        return Err(anyhow!("omp_session_scan_truncated"));
    }
    // Newest mtime wins if several.
    candidates.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(0)
    });
    Ok(candidates.pop())
}

/// Whether `candidate`'s REAL location is under `canon_root` (already
/// canonical).
///
/// This is the actual boundary. The component-by-component symlink rejections
/// above and in [`walkdir_jsonl`] are shape matching — they can only cover the
/// components that particular code happens to look at — whereas resolving the
/// winner and requiring containment holds for every intermediate component and
/// for `..` escapes at once.
///
/// It is unreachable through today's callers precisely BECAUSE those scans also
/// refuse symlinks, so no integration test can drive it without first weakening
/// them; the predicate is tested directly instead. It stays as the structural
/// guarantee those local checks are only approximating. Note it does NOT catch
/// hard links — `canonicalize` does not resolve them — so it is containment of
/// paths, not of inodes.
///
/// A path that cannot be canonicalized is NOT contained: when containment can't
/// be established, the honest answer is "not a candidate".
fn is_inside_canonical(candidate: &Path, canon_root: &Path) -> bool {
    std::fs::canonicalize(candidate)
        .map(|real| real.starts_with(canon_root))
        .unwrap_or(false)
}

/// Returns the hits plus whether any cap stopped the walk early — the caller
/// must not report a truncated walk as "not found".
fn walkdir_jsonl(root: &Path, session_id: &str) -> Result<(Vec<PathBuf>, bool)> {
    use std::collections::HashSet;
    /// Hard caps so a huge/unexpected `~/.omp/agent/sessions` tree cannot pin
    /// the async command thread or blow memory. Prefer the encoded-cwd bucket
    /// (caller); this walk is only the fallback.
    const MAX_DIRS: usize = 256;
    const MAX_FILES_SCANNED: usize = 4_096;
    const MAX_DEPTH: usize = 6;
    const MAX_HITS: usize = 32;
    const SKIP_DIR_NAMES: &[&str] = &[
        "node_modules", "target", ".git", "dist", "build", "cache", ".cache",
    ];
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut seen = HashSet::new();
    let mut dirs_visited = 0usize;
    let mut files_scanned = 0usize;
    // A cap stopped the walk with directories still queued: the answer is
    // "stopped looking", not "not there".
    let mut truncated = false;
    while let Some((dir, depth)) = stack.pop() {
        if dirs_visited >= MAX_DIRS || files_scanned >= MAX_FILES_SCANNED {
            truncated = true;
            break;
        }
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(canon) = dir.canonicalize() else {
            continue;
        };
        if !seen.insert(canon) {
            continue; // cycle / revisit
        }
        dirs_visited += 1;
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            if files_scanned >= MAX_FILES_SCANNED {
                truncated = true;
                break;
            }
            // Hitting the HIT cap is not truncation of the search space — the
            // session was found, several times over.
            if out.len() >= MAX_HITS {
                break;
            }
            let p = e.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue; // never follow symlink dirs/files into the walk
            }
            if meta.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if SKIP_DIR_NAMES.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                    continue;
                }
                // Cap enqueues, not only visits — a wide fan-out can otherwise
                // grow `stack` without bound before dirs_visited hits MAX_DIRS.
                if dirs_visited + stack.len() >= MAX_DIRS {
                    truncated = true;
                    continue;
                }
                stack.push((p, depth + 1));
            } else {
                files_scanned += 1;
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.contains(session_id) && name.ends_with(".jsonl") {
                    out.push(p);
                }
            }
        }
    }
    Ok((out, truncated))
}

/// omp session bucket: non-alnum path chars → `-` (observed
/// `~/.omp/agent/sessions/-workspace-weft-...`).
fn omp_encode_cwd(cwd: &Path) -> String {
    let s = cwd
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
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
    fn embedded_protocol_ids_are_rewritten_but_content_is_not() {
        // The old id appears: (1) as the sessionId field, (2) escaped inside a
        // hook payload as `\"session_id\":\"<old>\"` and as the transcript
        // path — all protocol, all rewritten; (3) pasted into visible user
        // text — content, deliberately NOT rewritten (the fork must not
        // disagree with the Weft timeline).
        let hook = format!(
            "{{\"type\":\"system\",\"sessionId\":\"sid-old\",\"hook\":\"in \\\"session_id\\\":\\\"sid-old\\\" and ~/.claude/projects/x/sid-old.jsonl ok\"}}"
        );
        let lines = vec![
            user_line("u1", "sid-old", "my session is sid-old, please remember it"),
            hook,
        ];
        let (dir, src) = fixture("sid-old", &lines);
        let new_id = fork_transcript_file(&src, "sid-old", &ClaudeCut::AfterUuid("u1".into()))
            .expect("fork")
            .expect("some");
        let out = read_fork(dir.path(), &new_id);
        // Protocol shapes rewritten.
        assert!(!out.contains("\\\"session_id\\\":\\\"sid-old\\\""), "escaped protocol id rewritten: {out}");
        assert!(out.contains(&format!("{new_id}.jsonl")), "transcript path rewritten: {out}");
        // Bare mention in user-visible text survives verbatim.
        assert!(
            out.contains("my session is sid-old, please remember it"),
            "content mention must NOT be rewritten: {out}"
        );
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

    /// Codex-review round 4: the native text match must use the DISPATCHED
    /// text (send() appends attachment instructions/spill paths), not the raw
    /// content.text — otherwise attachment-bearing messages never match.
    #[test]
    fn dispatched_text_rebuilds_attachment_appendices() {
        // Bare message: identity.
        let bare = r#"{"text":"hello","images":[],"files":[]}"#;
        assert_eq!(dispatched_text(false, 7, bare), "hello");
        assert_eq!(dispatched_text(true, 7, bare), "hello");
        // Files: same appendix for every dialect.
        let with_files = r#"{"text":"check this","images":[],"files":["src/a.rs","b.md"]}"#;
        let want_files = "check this\n\nAttached files (read them as needed):\n- src/a.rs\n- b.md\n";
        assert_eq!(dispatched_text(false, 7, with_files), want_files);
        assert_eq!(dispatched_text(true, 7, with_files), want_files);
        // Images: spilled only for per-turn dialects, keyed by the row id.
        let with_img = r#"{"text":"look","images":["data:image/png;base64,iVBOR"],"files":[]}"#;
        let tmp = std::env::temp_dir().join("weft-attachments");
        let want_img = format!(
            "look\n\nAttached images (read them as needed):\n- {}\n",
            tmp.join("msg7-0.png").display()
        );
        assert_eq!(dispatched_text(true, 7, with_img), want_img);
        assert_eq!(dispatched_text(false, 7, with_img), "look", "resident claude gets images inline");
        // Reconstructed text drives the native match: a stored opencode user
        // message holding the QUOTED dispatched form matches.
        let msgs = vec![oc_user("m1", &[&format!("\"{want_files}\"")])];
        assert_eq!(match_user_cut(&msgs, want_files, 1).as_deref(), Some("m1"));
        // Codex-review round 7: a persisted `dispatched` field (send() stamps
        // it for image-bearing rows, where spill failures are unknowable)
        // always wins over reconstruction.
        let stamped = r#"{"text":"look","images":["data:image/png;base64,iVBOR"],"files":[],"dispatched":"look\n\nAttached images (read them as needed):\n- /tmp/weft-attachments/msg7-0.png\n"}"#;
        assert_eq!(
            dispatched_text(true, 7, stamped),
            "look\n\nAttached images (read them as needed):\n- /tmp/weft-attachments/msg7-0.png\n"
        );
    }

    #[test]
    fn omp_first_user_is_row_order_not_match_count() {
        // Unrelated first user row must consume the "first" slot so a later
        // `notes\n\nrun tests` cannot match as system-prepend.
        let want = normalize_ws("run tests");
        assert!(!omp_user_body_matches("hello world", &want, Some("sys")));
        // After the first row is visited, a blank-paragraph body is not first.
        assert!(!omp_user_body_matches("notes\n\nrun tests", &want, None));
        // Exact later still matches.
        assert!(omp_user_body_matches("run tests", &want, None));
    }

    #[test]
    fn omp_multi_paragraph_system_prepend_matches() {
        let want = normalize_ws("run tests");
        let system = "You are the lead.\n\nOperate with judgment.\n\nThird para.";
        let body = format!("{system}\n\nrun tests");
        assert!(omp_user_body_matches(&body, &want, Some(system)));
        assert!(!omp_user_body_matches(&body, &want, None));
    }

    /// The prepend is STRIPPED, never guessed. Splitting on the last blank line
    /// broke both ways once a prompt had more than one paragraph.
    #[test]
    fn a_multi_paragraph_first_prompt_matches_whole_and_not_by_tail() {
        let system = "You are the lead.";
        let body = format!("{system}\n\nHello\n\nWorld");

        // The real target is the WHOLE user message.
        assert!(omp_user_body_matches(
            &body,
            &normalize_ws("Hello\n\nWorld"),
            Some(system)
        ));
        // A later message equal to the first prompt's final paragraph must not
        // steal the first turn's ordinal — that cut the whole session away.
        assert!(!omp_user_body_matches(
            &body,
            &normalize_ws("World"),
            Some(system)
        ));
    }

    /// A system prompt that changed since the session opened declines the
    /// relaxed match, so the caller reports "no anchor" instead of cutting at
    /// the wrong message.
    #[test]
    fn a_stale_system_prompt_declines_rather_than_mismatching() {
        let want = normalize_ws("run tests");
        let body = "You are helpful.\n\nrun tests";
        assert!(!omp_user_body_matches(body, &want, Some("Different prompt.")));
        assert!(!omp_user_body_matches(body, &want, Some("")));
        assert!(omp_user_body_matches(body, &want, Some("You are helpful.")));
    }

    #[test]
    fn omp_user_body_matches_system_prepend_only_on_first() {
        let want = normalize_ws("run tests");
        assert!(omp_user_body_matches(
            "You are helpful.\n\nrun tests",
            &want,
            Some("You are helpful.")
        ));
        assert!(omp_user_body_matches("run tests", &want, Some("sys")));
        // Later user with a blank paragraph must NOT match as a prepend.
        assert!(!omp_user_body_matches("notes\n\nrun tests", &want, None));
        assert!(omp_user_body_matches("run tests", &want, None));
        // Leading blank lines collapse under normalize_ws → exact match path.
        assert!(omp_user_body_matches("\n\nrun tests", &want, Some("sys")));
        // A non-empty prefix that is NOT the system prompt no longer matches
        // just because it shares a blank-line separator.
        assert!(!omp_user_body_matches("sys\n\nrun tests", &want, Some("other")));
        assert!(omp_user_body_matches("sys\n\nrun tests", &want, Some("sys")));
    }

    #[test]
    fn walkdir_jsonl_skips_symlinks_and_caps() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        // real hit
        std::fs::write(root.join("sess-abc.jsonl"), "{}\n").unwrap();
        // symlink file must be skipped
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(root.join("sess-abc.jsonl"), root.join("link-sess-abc.jsonl"));
            // outside is a SIBLING of root, not a child — only reachable via symlink
            let outside = dir.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("sess-abc-out.jsonl"), "{}\n").unwrap();
            let _ = std::os::unix::fs::symlink(&outside, root.join("symdir"));
        }
        // artifact dir skipped
        let nm = root.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("sess-abc-nm.jsonl"), "{}\n").unwrap();
        let (hits, _truncated) = walkdir_jsonl(&root, "sess-abc").expect("walk");
        let names: Vec<_> = hits.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        assert!(names.iter().any(|n| n == "sess-abc.jsonl"), "real hit: {names:?}");
        assert!(names.iter().all(|n| n != "link-sess-abc.jsonl"), "symlink file skipped: {names:?}");
        assert!(names.iter().all(|n| !n.contains("out")), "symlink dir not followed: {names:?}");
        assert!(names.iter().all(|n| n != "sess-abc-nm.jsonl"), "node_modules skipped: {names:?}");
    }

    #[test]
    fn preferred_omp_bucket_rejects_symlink_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        // Build a fake home-relative structure is hard; unit-test the metadata gate.
        let bucket = dir.path().join("bucket");
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("x-sid.jsonl"), "a\n").unwrap();
        let meta = std::fs::symlink_metadata(&bucket).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        #[cfg(unix)]
        {
            let link = dir.path().join("linkbucket");
            let _ = std::os::unix::fs::symlink(&bucket, &link);
            let lm = std::fs::symlink_metadata(&link).unwrap();
            assert!(lm.file_type().is_symlink());
            // Gate used by find_omp_session_file:
            let ok = std::fs::symlink_metadata(&link)
                .map(|m| m.is_dir() && !m.file_type().is_symlink())
                .unwrap_or(false);
            assert!(!ok, "symlink bucket must be rejected");
        }
    }

    #[test]
    fn walkdir_jsonl_respects_max_hits_cap() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path();
        // Create more matching files than MAX_HITS (32).
        for i in 0..80 {
            std::fs::write(root.join(format!("sess-cap-{i}.jsonl")), b"{}\n").unwrap();
        }
        let (hits, truncated) = walkdir_jsonl(root, "sess-cap").expect("walk");
        assert!(hits.len() <= 32, "capped at MAX_HITS, got {}", hits.len());
        assert!(!hits.is_empty());
        assert!(
            !truncated,
            "stopping at the HIT cap is not a truncated search — the session was found"
        );
    }

    #[test]
    fn walkdir_jsonl_caps_wide_directory_fanout() {
        use std::fs;
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path();
        // One level with many child dirs (few files) — must not grow stack unbounded.
        for i in 0..2_000 {
            let d = root.join(format!("d{i}"));
            fs::create_dir(&d).expect("mkdir");
            // one non-matching file per dir so we exercise file branch too
            fs::write(d.join("x.txt"), b"x").expect("file");
        }
        // A single hit buried late should still be findable within caps, but
        // the walk must terminate quickly and never enqueue > MAX_DIRS pending.
        fs::write(root.join("sess-wide.jsonl"), b"{}\n").expect("hit");
        let (hits, _truncated) = walkdir_jsonl(root, "sess-wide").expect("walk");
        // Cap may stop before discovering the hit if fan-out exhausts MAX_DIRS;
        // the invariant under test is termination + no panic/OOM, and hits ≤ MAX_HITS.
        assert!(hits.len() <= 32, "hits capped");
    }

    /// CLAUDE.md requires symlink-containment coverage for recursive
    /// filesystem work. `is_dir()` FOLLOWS a symlink, and the per-bucket and
    /// per-entry checks only reject one at the final component — so a
    /// redirected root put the whole scan outside `~/.omp/agent/sessions`, and
    /// a rewind would then write its fork beside that external file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_session_root_is_refused() {
        use std::fs;
        let _serialized = home_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();

        // The real session tree lives OUTSIDE the home we will point at.
        let outside = dir.path().join("elsewhere");
        let bucket = outside.join(omp_encode_cwd(&cwd));
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join("zzz_sess-outside.jsonl"), b"{}\n").unwrap();

        // ~/.omp/agent/sessions -> the outside tree.
        fs::create_dir_all(home.join(".omp/agent")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(".omp/agent/sessions")).unwrap();

        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let found = find_omp_session_file(&cwd, "sess-outside");
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(found, Ok(None)),
            "a symlinked session root must select nothing, got {found:?}"
        );
    }

    /// The containment check is on REAL locations, so an entry reachable only
    /// through a symlinked BUCKET (an intermediate component, which the
    /// per-entry check never looks at) is refused too.
    #[cfg(unix)]
    #[test]
    fn a_session_reached_through_a_symlinked_bucket_is_refused() {
        use std::fs;
        let _serialized = home_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();

        let outside = dir.path().join("outside-bucket");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("zzz_sess-linked.jsonl"), b"{}\n").unwrap();

        let sessions = home.join(".omp/agent/sessions");
        fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&outside, sessions.join(omp_encode_cwd(&cwd))).unwrap();

        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let found = find_omp_session_file(&cwd, "sess-linked");
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            matches!(found, Ok(None)),
            "a symlinked bucket must not yield a candidate, got {found:?}"
        );
    }

    /// The boundary predicate itself. Unreachable via `find_omp_session_file`
    /// today (its scans refuse symlinks first), so it is exercised directly —
    /// a guard with no test is indistinguishable from one that always returns
    /// the same answer.
    #[cfg(unix)]
    #[test]
    fn containment_is_of_real_locations_not_path_strings() {
        use std::fs;
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("escaped.jsonl"), b"{}\n").unwrap();
        fs::write(root.join("real.jsonl"), b"{}\n").unwrap();
        let canon_root = fs::canonicalize(&root).unwrap();

        assert!(is_inside_canonical(&root.join("real.jsonl"), &canon_root));

        // Reachable through a path that LOOKS inside, but resolves outside.
        std::os::unix::fs::symlink(outside.join("escaped.jsonl"), root.join("looks-inside.jsonl"))
            .unwrap();
        assert!(
            !is_inside_canonical(&root.join("looks-inside.jsonl"), &canon_root),
            "a symlink pointing out of the tree is not contained"
        );

        // `..` traversal that stringly-starts-with the root.
        assert!(!is_inside_canonical(
            &root.join("../outside/escaped.jsonl"),
            &canon_root
        ));

        // Cannot be canonicalized → not contained.
        assert!(!is_inside_canonical(&root.join("missing.jsonl"), &canon_root));
    }

    /// "Stopped looking" must not be reported as "not there". The previous
    /// large-directory test only asserted `is_ok()`, which accepts exactly that
    /// false negative: a bucket past the scan cap made `fork_omp_at` answer
    /// `omp_session_not_found` for a session that exists.
    #[test]
    fn a_truncated_scan_is_a_distinct_error_not_a_missing_session() {
        use std::fs;
        let _serialized = home_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let bucket = home.join(".omp/agent/sessions").join(omp_encode_cwd(&cwd));
        fs::create_dir_all(&bucket).unwrap();
        // Past MAX_PREFERRED_SCAN (4096) with NO matching file anywhere, so the
        // fallback walk is capped too and the session is genuinely unreachable
        // within the caps.
        for i in 0..5_000 {
            fs::write(bucket.join(format!("noise-{i}.txt")), b"x").unwrap();
        }

        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let found = find_omp_session_file(&cwd, "sess-past-the-cap");
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let err = found.expect_err("a truncated scan must not answer Ok(None)");
        assert!(
            err.to_string().contains("omp_session_scan_truncated"),
            "got {err}"
        );
    }

    /// The guards must not reject the ordinary case.
    #[test]
    fn an_ordinary_session_file_is_still_found() {
        use std::fs;
        let _serialized = home_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let bucket = home.join(".omp/agent/sessions").join(omp_encode_cwd(&cwd));
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join("zzz_sess-ok.jsonl"), b"{}\n").unwrap();

        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let found = find_omp_session_file(&cwd, "sess-ok");
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let path = found.expect("lookup ok").expect("a candidate");
        assert!(path.ends_with("zzz_sess-ok.jsonl"), "got {path:?}");
    }

    #[test]
    fn preferred_bucket_caps_large_dir() {
        use std::fs;
        let _serialized = home_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let sessions = home.join(".omp/agent/sessions");
        let cwd = home.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let encoded = omp_encode_cwd(&cwd);
        let bucket = sessions.join(&encoded);
        fs::create_dir_all(&bucket).unwrap();
        for i in 0..5_000 {
            fs::write(bucket.join(format!("noise-{i}.txt")), b"x").unwrap();
        }
        fs::write(bucket.join("zzz_sess-cap-me.jsonl"), b"{}\n").unwrap();
        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let found = find_omp_session_file(&cwd, "sess-cap-me");
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert!(found.is_ok());
    }

    /// The system prepend the first ACP user turn carries in these fixtures.
    const SYS: &str = "SYSTEM PREAMBLE";

    /// Serializes the tests that override `HOME`. `find_omp_session_file`
    /// resolves it through `dirs::home_dir()` and cargo runs tests on threads,
    /// so without this two HOME-swapping tests interleave and each sees the
    /// other's root — a flake that only shows up under load.
    fn home_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn omp_user_line(blocks: serde_json::Value) -> String {
        serde_json::json!({
            "type": "message",
            "message": { "role": "user", "content": blocks }
        })
        .to_string()
    }

    fn omp_assistant_line(text: &str) -> String {
        serde_json::json!({
            "type": "message",
            "message": { "role": "assistant", "content": [{"type":"text","text":text}] }
        })
        .to_string()
    }

    /// What omp actually records for an image-only message: `prompt()` always
    /// emits a text block, so the text is EMPTY rather than absent.
    fn image_only_line() -> String {
        omp_user_line(serde_json::json!([
            {"type":"text","text":""},
            {"type":"image","mimeType":"image/png","data":"AAAA"}
        ]))
    }

    /// Tool results are role=user too, but carry no text block — they must
    /// never be mistaken for an (empty-bodied) user prompt.
    #[test]
    fn omp_cut_skips_tool_results_which_have_no_text_block() {
        let tool_result = omp_user_line(serde_json::json!([
            {"type":"tool_result","tool_use_id":"t1","content":"ok"}
        ]));
        let lines: Vec<&str> = vec![&tool_result];

        assert_eq!(
            omp_cut_index(&lines, "", 1, SYS),
            None,
            "a tool_result row is not an empty user prompt"
        );
    }

    /// The whole upstream path for a NON-FIRST image-only rewind: the DB-side
    /// ordinal has to survive into the transcript cut. A matcher-level check
    /// cannot catch this — `ordinal_of` zeroed the empty target and the engine
    /// rejected the rewind as "no anchor" before the matcher ever ran.
    #[test]
    fn image_only_omp_prompt_rewinds_end_to_end() {
        let first = omp_user_line(serde_json::json!([{"type":"text","text":"hello"}]));
        let reply = omp_assistant_line("hi");
        let tool_result = omp_user_line(serde_json::json!([
            {"type":"tool_result","tool_use_id":"t1","content":"ok"}
        ]));
        let image_only = image_only_line();
        let after = omp_assistant_line("nice picture");
        let lines: Vec<&str> = vec![&first, &reply, &tool_result, &image_only, &after];

        // The engine reconstructs these dispatched texts from the DB rows; the
        // image-only row's is empty because ACP keeps images out of the text.
        let dispatched = vec!["hello".to_string(), String::new()];
        assert_eq!(
            ordinal_of(&dispatched, ""),
            0,
            "claude's transcript cannot address an empty prompt"
        );
        let ordinal = ordinal_of_prompt(&dispatched, "");
        assert_eq!(ordinal, 1, "ACP can: it is the first empty-bodied prompt");

        assert_eq!(
            omp_cut_index(&lines, "", ordinal, SYS),
            Some(3),
            "cut before the image-only prompt itself"
        );
    }

    /// Two image-only messages share the empty identity, so the ordinal is the
    /// only thing telling them apart — it must select the right one.
    #[test]
    fn the_ordinal_picks_between_two_image_only_prompts() {
        let first = omp_user_line(serde_json::json!([{"type":"text","text":"hello"}]));
        let a = image_only_line();
        let reply = omp_assistant_line("one");
        let b = image_only_line();
        let lines: Vec<&str> = vec![&first, &a, &reply, &b];

        let dispatched = vec!["hello".to_string(), String::new(), String::new()];
        assert_eq!(ordinal_of_prompt(&dispatched, ""), 2, "counts up to the target");

        assert_eq!(omp_cut_index(&lines, "", 1, SYS), Some(1));
        assert_eq!(omp_cut_index(&lines, "", 2, SYS), Some(3));
        assert_eq!(omp_cut_index(&lines, "", 3, SYS), None, "only two exist");
    }

    /// A rewind of the first post-ENGINE-SWITCH message does not match, and
    /// that is pinned deliberately.
    ///
    /// The dispatched text is `system + context_digest + user`, while the
    /// persisted row holds only `user` — the digest is kept out of the DB on
    /// purpose (PR #139: it can carry paths and pasted secrets). Stripping the
    /// system prompt still leaves `digest + user`, so nothing matches.
    ///
    /// No match is the SAFE outcome: `fork_omp_at` runs before any stop or
    /// truncate, so the rewind fails with the session fully intact. The tempting
    /// "fix" — accepting any trailing segment after a blank line — is the
    /// mis-cut this matcher was tightened to prevent one round earlier. A real
    /// fix has to anchor the OMP-native history boundary (the jsonl starts at
    /// the switch, while the DB ordinal counts the whole session), which is
    /// cross-engine rewind semantics, not a matcher tweak.
    #[test]
    fn a_post_switch_first_prompt_does_not_match_rather_than_guessing() {
        let dispatched = format!("{SYS}\n\nCONTEXT DIGEST: 12 prior turns…\n\nrun the tests");
        let first = omp_user_line(serde_json::json!([{"type":"text","text":dispatched}]));
        let lines: Vec<&str> = vec![&first];

        assert_eq!(
            omp_cut_index(&lines, "run the tests", 1, SYS),
            None,
            "refusing to guess the digest boundary is correct; a match here would \
             mean accepting any trailing segment, which mis-cuts multi-paragraph prompts"
        );
    }

    /// The first prompt carries the system prepend, whose relaxed match must
    /// not swallow an empty target — that would cut the whole session away.
    #[test]
    fn a_system_prepended_first_prompt_never_matches_an_empty_target() {
        let first = omp_user_line(serde_json::json!([
            {"type":"text","text":format!("{SYS}\n\nhello")}
        ]));
        let image_only = image_only_line();
        let lines: Vec<&str> = vec![&first, &image_only];

        assert_eq!(
            omp_cut_index(&lines, "", 1, SYS),
            Some(1),
            "the empty target is the image-only prompt, not the prepended first turn"
        );
        // And the prepended turn IS reachable by its own text.
        assert_eq!(omp_cut_index(&lines, "hello", 1, SYS), Some(0));
    }
}
