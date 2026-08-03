//! Map ACP `session/update` payloads onto Weft `ChatEvent` / side channels.
//!
//! Backend-agnostic: no CLI identity strings. Thinking chunks are dropped in v1.

use serde_json::Value;

use crate::lead_chat::proto::{cap_and_dedup_images, ChatEvent, SlashCmd, ToolCall, ToolResultItem};

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
    /// Streaming model reasoning (agent_thought_chunk). Not final answer text.
    Thought {
        text: String,
    },
    /// Non-terminal tool progress for live activity telemetry.
    ToolProgress {
        summary: String,
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
        "agent_thought_chunk" => thought_chunk(update),
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
        agent_thread: None,
    })
}

fn thought_chunk(update: &Value) -> UpdateOut {
    let text = update
        .pointer("/content/text")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return UpdateOut::Ignore;
    }
    UpdateOut::Thought {
        text: text.to_string(),
    }
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
            collab_threads: Vec::new(),
            // A call's own start never carries a result yet — see
            // `proto::ToolCall::images`.
            images: Vec::new(),
        }],
        uuid: None,
        agent_thread: None,
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
        // `write` belongs with the other mutations. Collapsing it to `read`
        // labelled a file write as a read in the timeline — wrong name, wrong
        // icon — while the permission layer classified the same kind as a
        // write, so the two surfaces disagreed about the same tool call.
        if matches!(kind, "edit" | "write" | "delete" | "move") {
            return kind.to_string();
        }
        return "read".into();
    }
    kind.to_string()
}

fn tool_call_update(update: &Value) -> UpdateOut {
    let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");
    // Only terminal states become ToolResults; in_progress keeps the turn alive.
    if status != "completed" && status != "failed" {
        let title = update
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("tool");
        return UpdateOut::ToolProgress {
            summary: title.to_string(),
        };
    }
    let id = update
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return UpdateOut::Ignore;
    }
    // the count/size cap is applied
    // WHILE `extract_tool_images` scans the source array now, not afterward.
    // that streaming cap
    // (`cap_and_dedup_images`, now `lead_chat::proto`) is the SAME function
    // the claude dialect calls too — see its own doc for why the OLD "format
    // everything, dedup everything, cap afterward" order let an
    // oversized/excessive result (either dialect) allocate hundreds of MB
    // before a single image was ever dropped. Any it drops are announced in
    // `output`, the SAME place the equivalent claude-side note lands —
    // `note_omitted_images`'s wording doesn't care which dialect produced
    // the count.
    let (images, dropped) = extract_tool_images(update);
    let output = crate::lead_chat::proto::note_omitted_images(
        crate::lead_chat::proto::cap_output(extract_tool_output(update)),
        dropped,
    );
    let is_error = status == "failed";
    UpdateOut::Chat(ChatEvent::ToolResults {
        items: vec![ToolResultItem {
            id,
            output,
            is_error,
            collab_threads: Vec::new(),
            images,
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

/// Image content blocks in a `tool_call_update` — the counterpart to
/// `extract_tool_output`'s text extraction, scanning the exact same two
/// sources it does (`rawOutput.content` array items directly, and the
/// update's own `content[]` — trying both a flat block and content[]'s
/// nested `.content` wrapper, mirroring how `extract_tool_output` tries both
/// `item.text` and `item.content.text`) so an image sitting in either shape
/// is found. `{"type":"image","data":d,"mimeType":m}` → `data:<m>;base64,<d>`.
/// Returns `(kept, dropped)` — `dropped` counts every DISTINCT image
/// candidate this omitted for being oversized or in excess of the count cap
/// (see [`cap_and_dedup_images`]'s own doc); the caller (`tool_call_update`)
/// turns that into the SAME "(N image(s) omitted)" note the claude dialect
/// produces — both dialects share this ONE capper
/// P2.
///
/// This closes a real gap: before this function existed, an omp screenshot
/// tool's image block was silently dropped — `extract_tool_output` only ever
/// read the text half of a `tool_call_update`'s content.
///
/// This used to scan BOTH sources and concatenate
/// whatever each one found — but a single tool result that mirrors the same
/// image into both `rawOutput.content` AND `content[]` (a real shape at
/// least one ACP backend produces) would then carry it TWICE, doubling the
/// payload for no new information. Now PRIORITIZED exactly like
/// `extract_tool_output`'s own text extraction: `rawOutput.content` wins
/// outright when it carries at least one image (kept OR dropped — a source
/// that carried nothing BUT oversized images still "carried an image" for
/// this priority decision, exactly like the pre-capper behavior); `content[]`
/// is only consulted as a fallback when `rawOutput.content` carried none at
/// all — never merged.
///
/// the size/count cap used to apply
/// AFTER this function had already formatted EVERY block into a full data
/// URI string and cloned each distinct one into a `HashSet` for dedup — a
/// malicious/broken ACP result with hundreds of near-2MB image blocks would
/// allocate/copy hundreds of MB before a single one was ever dropped. The cap
/// now applies WHILE scanning, via [`cap_and_dedup_images`] — see its own doc
/// (moved to `lead_chat::proto` so the claude
/// dialect can share this exact implementation instead of keeping its own
/// copy; `image_block_fields` below is still ACP's own resolve fn, passed
/// into the shared capper same as before) for how it bounds both the count
/// of survivors AND the work spent getting there.
fn extract_tool_images(update: &Value) -> (Vec<String>, usize) {
    if let Some(arr) = update.pointer("/rawOutput/content").and_then(|c| c.as_array()) {
        let (kept, dropped) = cap_and_dedup_images(arr.iter(), image_block_fields);
        if !kept.is_empty() || dropped > 0 {
            return (kept, dropped);
        }
    }
    match update.get("content").and_then(|c| c.as_array()) {
        Some(arr) => cap_and_dedup_images(arr.iter(), |item| {
            item.get("content")
                .and_then(image_block_fields)
                .or_else(|| image_block_fields(item))
        }),
        None => (Vec::new(), 0),
    }
}

/// Borrowed `(mime, data)` fields for a `{"type":"image",…}` content block —
/// the recognize-and-extract half of what used to be [`image_block_data_uri`]
/// (now just the format step below), split out so [`cap_and_dedup_images`]
/// can dedup/size-check BEFORE ever formatting/allocating the eventual owned
/// data URI string.
fn image_block_fields(block: &Value) -> Option<(&str, &str)> {
    if block.get("type").and_then(|t| t.as_str()) != Some("image") {
        return None;
    }
    let mime = block.get("mimeType").and_then(|m| m.as_str())?;
    let data = block.get("data").and_then(|d| d.as_str())?;
    Some((mime, data))
}

fn commands(update: &Value) -> UpdateOut {
    let Some(arr) = update
        .get("availableCommands")
        .and_then(|a| a.as_array())
    else {
        return UpdateOut::Ignore;
    };
    // Empty array is authoritative: clear the engine's cached slash list.
    if arr.is_empty() {
        return UpdateOut::Commands(Vec::new());
    }
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
    /// A file write must stay a write in the timeline. Collapsing `write` into
    /// the `read` fallback gave it a read's name and icon while the permission
    /// layer classified the same call as a mutation — two surfaces disagreeing
    /// about one tool call.
    #[test]
    fn a_write_tool_call_is_not_labelled_read() {
        let raw = serde_json::json!({ "path": "src/main.rs" });
        for kind in ["edit", "write", "delete", "move"] {
            assert_eq!(
                super::tool_name_from_raw(Some(&raw), kind),
                kind,
                "{kind} is a mutation"
            );
        }
        assert_eq!(super::tool_name_from_raw(Some(&raw), "read"), "read");
        // A command still wins over any path-bearing input.
        let cmd = serde_json::json!({ "command": "ls" });
        assert_eq!(super::tool_name_from_raw(Some(&cmd), "write"), "bash");
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn message_chunk_to_text_delta() {
        let u = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "pong" }
        });
        match update_to_out(&u) {
            UpdateOut::Chat(ChatEvent::TextDelta { text, item: None, .. }) => {
                assert_eq!(text, "pong");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn thought_chunk_maps() {
        let u = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" }
        });
        match update_to_out(&u) {
            UpdateOut::Thought { text } => assert_eq!(text, "hmm"),
            o => panic!("{o:?}"),
        }
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
                // A text-only completion carries no images at all.
                assert!(items[0].images.is_empty());
            }
            o => panic!("{o:?}"),
        }

        let inflight = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-1",
            "status": "in_progress"
        });
        assert!(matches!(update_to_out(&inflight), UpdateOut::ToolProgress { .. }));
    }

    /// A tool_call_update carrying an image block (e.g. an omp screenshot
    /// tool) must land it in `items[0].images` — this was the real gap: before
    /// `extract_tool_images` existed, `extract_tool_output` only ever read the
    /// text half of `rawOutput.content`/`content[]`, so the image block was
    /// silently dropped.
    #[test]
    fn tool_call_update_with_image_block_populates_images() {
        let done = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-2",
            "status": "completed",
            "rawOutput": {
                "content": [
                    { "type": "text", "text": "screenshot taken" },
                    { "type": "image", "data": "QUJD", "mimeType": "image/png" }
                ]
            }
        });
        match update_to_out(&done) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(items[0].output, "screenshot taken");
                assert_eq!(items[0].images, vec!["data:image/png;base64,QUJD".to_string()]);
            }
            o => panic!("{o:?}"),
        }

        // The nested `content[].content` wrapper variant (the shape
        // `extract_tool_output`'s text half already accommodates for text
        // blocks) must be recognized for images too.
        let nested = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-3",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "image", "data": "WFla", "mimeType": "image/jpeg" } }
            ]
        });
        match update_to_out(&nested) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(items[0].images, vec!["data:image/jpeg;base64,WFla".to_string()]);
            }
            o => panic!("{o:?}"),
        }

        // A failed call with more than 4 images: only the first 4 survive and
        // the rest are announced as omitted in the output text
        // (cap_and_dedup_images, literally shared with the claude dialect
        // shared with the claude dialect).
        let overflow_images: Vec<Value> = (0..5)
            .map(|i| json!({ "type": "image", "data": format!("img{i}"), "mimeType": "image/png" }))
            .collect();
        let overflow = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-4",
            "status": "failed",
            "rawOutput": { "content": overflow_images }
        });
        match update_to_out(&overflow) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(items[0].images.len(), 4);
                assert!(items[0].is_error);
                assert!(items[0].output.ends_with("(1 image(s) omitted: too large)"));
            }
            o => panic!("{o:?}"),
        }
    }

    /// A tool result that mirrors the SAME image into
    /// BOTH `rawOutput.content` and `content[]` (a real shape at least one
    /// ACP backend produces) must land ONE image, not two — `rawOutput.
    /// content` wins outright (mirroring `extract_tool_output`'s own text
    /// priority) and `content[]` is never even consulted once it carried at
    /// least one image, so this is not just a dedup of equal strings but a
    /// genuine "only scan one source" priority rule.
    #[test]
    fn tool_call_update_mirrored_into_both_sources_yields_one_image() {
        let mirrored = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-5",
            "status": "completed",
            "rawOutput": {
                "content": [
                    { "type": "image", "data": "AAAA", "mimeType": "image/png" }
                ]
            },
            "content": [
                { "type": "content", "content": { "type": "image", "data": "AAAA", "mimeType": "image/png" } }
            ]
        });
        match update_to_out(&mirrored) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(
                    items[0].images,
                    vec!["data:image/png;base64,AAAA".to_string()],
                    "the SAME image mirrored into both sources must appear exactly once"
                );
            }
            o => panic!("{o:?}"),
        }
    }

    /// The second, independent safety net: even ONE source repeating the
    /// identical image twice (a backend quirk, not the two-source mirroring
    /// case above) must still collapse to one — `extract_tool_images`'s own
    /// dedup runs on the final list regardless of which source it came from.
    #[test]
    fn tool_call_update_repeats_within_one_source_still_dedup() {
        let repeated = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call-6",
            "status": "completed",
            "rawOutput": {
                "content": [
                    { "type": "image", "data": "BBBB", "mimeType": "image/png" },
                    { "type": "image", "data": "BBBB", "mimeType": "image/png" },
                    { "type": "image", "data": "CCCC", "mimeType": "image/png" }
                ]
            }
        });
        match update_to_out(&repeated) {
            UpdateOut::Chat(ChatEvent::ToolResults { items }) => {
                assert_eq!(
                    items[0].images,
                    vec![
                        "data:image/png;base64,BBBB".to_string(),
                        "data:image/png;base64,CCCC".to_string(),
                    ],
                    "a repeated data URI within one source collapses; distinct ones are kept, in order"
                );
            }
            o => panic!("{o:?}"),
        }
    }

    /// a source carrying MORE than the
    /// 4-image cap, where a 5th VALID (not oversized, not a duplicate) image
    /// sits right after the cap already filled, must still cap at 4 — proving
    /// the count cap is enforced by COUNT alone, not merely as a side effect
    /// of size/dedup filtering.
    #[test]
    fn extract_tool_images_caps_by_count_even_when_every_block_is_small_and_distinct() {
        let mut blocks: Vec<Value> = (0..4)
            .map(|i| json!({"type":"image","mimeType":"image/png","data": format!("img{i}")}))
            .collect();
        blocks.push(json!({"type":"image","mimeType":"image/png","data":"img4-excess"}));
        let update = json!({"rawOutput": {"content": blocks}});
        let (images, dropped) = extract_tool_images(&update);
        assert_eq!(
            images,
            vec![
                "data:image/png;base64,img0",
                "data:image/png;base64,img1",
                "data:image/png;base64,img2",
                "data:image/png;base64,img3",
            ]
        );
        assert_eq!(dropped, 1);
    }

    /// A tiny counting wrapper around a slice iterator — used to observe
    /// exactly how many times [`cap_and_dedup_images`] calls `.next()` on its
    /// source. Holds an external `&Cell<usize>` (not an owned counter) so the
    /// count survives after the wrapper itself is consumed/dropped inside the
    /// function under test.
    struct CountingIter<'a> {
        inner: std::slice::Iter<'a, Value>,
        count: &'a std::cell::Cell<usize>,
    }
    impl<'a> Iterator for CountingIter<'a> {
        type Item = &'a Value;
        fn next(&mut self) -> Option<Self::Item> {
            self.count.set(self.count.get() + 1);
            self.inner.next()
        }
    }

    /// A prior flaw in [`cap_and_dedup_images`] proved it
    /// never even `.next()`-ed the source once the count cap was full — round-
    /// 13 P2 deliberately gives that property up FOR THE COUNTING
    /// PHASE ONLY: the old `dropped += blocks.len()` counted every leftover
    /// block, images or not, as an omitted image (see the trailing-text tests
    /// below), so the remainder must now be visited via `resolve` to tell an
    /// image candidate from anything else. What still must NEVER happen is
    /// `format!`-ing (allocating) a data URI for anything beyond the 4th
    /// survivor — `kept.len()` staying at 4 here, even with 50 more valid,
    /// distinct images available, is exactly that guarantee.
    #[test]
    fn cap_and_dedup_images_visits_the_remainder_to_count_but_never_formats_past_the_cap() {
        let mut blocks: Vec<Value> = (0..4)
            .map(|i| json!({"type":"image","mimeType":"image/png","data": format!("img{i}")}))
            .collect();
        for i in 0..50 {
            blocks.push(json!({"type":"image","mimeType":"image/png","data": format!("excess{i}")}));
        }
        let counter = std::cell::Cell::new(0usize);
        let iter = CountingIter {
            inner: blocks.iter(),
            count: &counter,
        };
        let (kept, dropped) = cap_and_dedup_images(iter, image_block_fields);
        assert_eq!(
            kept.len(),
            4,
            "format! must never run past the count cap, regardless of how many more valid images follow"
        );
        assert_eq!(
            counter.get(),
            55,
            "the remainder IS visited (via `resolve` only, never `format!`) so \
             `dropped` can tell a real image from plain text or a duplicate — 4 calls to fill \
             `kept`, then 50 more `Some` calls for the remainder, plus the final `None` call that \
             ends the `for` loop"
        );
        assert_eq!(dropped, 50, "all 50 are distinct, non-duplicate images, so every one counts");
    }

    /// once the 4-image cap is already
    /// full, trailing TEXT blocks are not images and must not inflate
    /// `dropped` — the bug this change fixes: the old code counted
    /// `blocks.len()`, which conflated every leftover block (image or not)
    /// with an omitted image.
    #[test]
    fn cap_and_dedup_images_does_not_count_trailing_text_blocks_as_dropped_images() {
        let mut blocks: Vec<Value> = (0..4)
            .map(|i| json!({"type":"image","mimeType":"image/png","data": format!("img{i}")}))
            .collect();
        blocks.push(json!({"type":"text","text":"note one"}));
        blocks.push(json!({"type":"text","text":"note two"}));
        let (kept, dropped) = cap_and_dedup_images(blocks.iter(), image_block_fields);
        assert_eq!(kept.len(), 4);
        assert_eq!(dropped, 0, "trailing text blocks are not images and must not inflate dropped");
    }

    /// The counterpart to the text-only case above: a genuine 5th (valid,
    /// distinct) image sitting right after the 4-image cap already filled,
    /// followed by a trailing text block, must still be counted as one
    /// omitted image — this fixes the false positive on text without
    /// introducing a false negative on a real excess image.
    #[test]
    fn cap_and_dedup_images_still_counts_a_genuine_excess_image_before_trailing_text() {
        let mut blocks: Vec<Value> = (0..5)
            .map(|i| json!({"type":"image","mimeType":"image/png","data": format!("img{i}")}))
            .collect();
        blocks.push(json!({"type":"text","text":"note"}));
        let (kept, dropped) = cap_and_dedup_images(blocks.iter(), image_block_fields);
        assert_eq!(kept.len(), 4);
        assert_eq!(dropped, 1);
    }

    /// Two IDENTICAL excess images (repeating each other, not merely
    /// repeating a kept one) collapse to a single `dropped` — mirrors the
    /// "a duplicate silently collapses, never counted twice" rule the scan
    /// already applies to the first 4 survivors. Chosen deliberately: the
    /// contract is "remaining distinct image candidates", and collapsing
    /// duplicates in the remainder is what keeps that word meaningful.
    #[test]
    fn cap_and_dedup_images_counts_duplicate_excess_images_once() {
        let mut blocks: Vec<Value> = (0..4)
            .map(|i| json!({"type":"image","mimeType":"image/png","data": format!("img{i}")}))
            .collect();
        blocks.push(json!({"type":"image","mimeType":"image/png","data":"dup-excess"}));
        blocks.push(json!({"type":"image","mimeType":"image/png","data":"dup-excess"}));
        let (kept, dropped) = cap_and_dedup_images(blocks.iter(), image_block_fields);
        assert_eq!(kept.len(), 4);
        assert_eq!(dropped, 1, "two identical excess images collapse to a single dropped count");
    }

    /// A single oversized block (over `cap_and_dedup_images`'s 2,000,000-char
    /// threshold) is dropped without ever being kept — the size half of the
    /// cap, independent of the count half covered above.
    #[test]
    fn extract_tool_images_drops_a_single_oversized_block() {
        let big = "x".repeat(2_000_001);
        let update = json!({"rawOutput": {"content": [
            {"type":"image","mimeType":"image/png","data": big}
        ]}});
        let (images, dropped) = extract_tool_images(&update);
        assert!(images.is_empty());
        assert_eq!(dropped, 1);
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
    fn empty_available_commands_clears() {
        let u = json!({"sessionUpdate":"available_commands_update","availableCommands":[]});
        match update_to_out(&u) {
            UpdateOut::Commands(c) => assert!(c.is_empty()),
            other => panic!("expected empty Commands, got {other:?}"),
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
