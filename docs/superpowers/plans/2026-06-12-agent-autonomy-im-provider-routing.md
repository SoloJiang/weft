# Agent Autonomy IM Provider Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor Weft agent prompts around autonomous role contracts and make Concierge route IM-created or IM-intervened issues into provider-native topics/threads.

**Architecture:** Add small backend units rather than expanding prompt strings: role prompt rendering stays in `lead_chat/commands.rs` and `brief.rs`, IM provider capability/context helpers live in `im/mod.rs`, and product-level IM-aware global tools live in `bus/global.rs`. The first provider is Feishu, but Concierge sees provider capabilities instead of Feishu-specific prompt rules.

**Tech Stack:** Rust 2021, Tauri commands, SeaORM SQLite store, existing IM `Channel` trait, existing MCP JSON tool handlers, existing Rust tests plus `pnpm build`.

---

## File Structure

- Modify: `src-tauri/src/lead_chat/commands.rs`
  - Replace Lead fixed-sequence prompt with scope-convergence policy.
  - Replace Concierge Feishu-specific prompt with provider-aware routing policy.
  - Keep public function names `lead_prompt`, `concierge_prompt`, and `lang_directive` stable.
  - Update prompt unit tests or add new tests near the existing `lead_state_label` tests if no prompt tests exist in this file.

- Modify: `src-tauri/src/brief.rs`
  - Change worker brief status/mandate prose from workflow script to delivery contract.
  - Preserve `BriefData`, `RepoBrief`, and `assemble` signatures.
  - Update existing brief tests for the new contract text.

- Modify: `src-tauri/src/im/mod.rs`
  - Add `ImProviderCapabilities` and a Feishu capability constructor.
  - Add structured IM context rendering.
  - Change `consume_free_text` to send `<weft:im_context>` and `<weft:user_message>` instead of inline `feishu_chat_id` framing.
  - Add helper functions for IM-aware issue thread ensurement that can be reused by global tools.

- Modify: `src-tauri/src/bus/global.rs`
  - Add global MCP tools `create_issue_from_im` and `ensure_issue_im_thread`.
  - Keep existing `create_issue`, `ensure_issue_topic`, and `message_lead` available.
  - Add tests for new tools and update `global_specs` tests if present.

- Test: existing Rust tests in `src-tauri/src/brief.rs`, `src-tauri/src/lead_chat/commands.rs`, `src-tauri/src/im/mod.rs`, `src-tauri/src/bus/global.rs`.

---

### Task 1: Make prompts policy-based

**Files:**
- Modify: `src-tauri/src/lead_chat/commands.rs:38-127`
- Test: `src-tauri/src/lead_chat/commands.rs`

- [ ] **Step 1: Add failing prompt tests**

Add tests in `src-tauri/src/lead_chat/commands.rs` under the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn lead_prompt_is_policy_not_fixed_sequence() {
    let prompt = super::lead_prompt();
    assert!(prompt.contains("converge the issue's write scope"));
    assert!(prompt.contains("Use task and repo-map capabilities when they materially affect scope"));
    assert!(!prompt.contains("Start by greeting"));
    assert!(!prompt.contains("call get_task"));
}

#[test]
fn concierge_prompt_is_provider_aware_not_feishu_scripted() {
    let prompt = super::concierge_prompt("zh");
    assert!(prompt.contains("IM provider"));
    assert!(prompt.contains("provider-native"));
    assert!(prompt.contains("创建并绑定"));
    assert!(!prompt.contains("feishu_chat_id"));
    assert!(!prompt.contains("ensure_issue_topic"));
}
```

- [ ] **Step 2: Run failing tests**

Run:

```bash
cd src-tauri && cargo test lead_prompt_is_policy_not_fixed_sequence concierge_prompt_is_provider_aware_not_feishu_scripted
```

Expected: both tests fail because current prompts still contain fixed Lead sequencing and Feishu-specific Concierge instructions.

- [ ] **Step 3: Replace Lead prompt text**

Replace `BASE_PROMPT` with policy text that preserves hard constraints:

```rust
const BASE_PROMPT: &str = "You are the lead for this thread in weft — the human's main collaborator for converging write scope. \
Your mission is to converge the issue's write scope with the human, then propose worker directions. \
Use the weft_planner MCP capabilities when they materially affect scope: read the task when the request is unclear, and read the repo map when repo ownership or cross-repo dependencies matter. \
Do not write code, and do not plan the directions' implementations — each worker decides how to deliver its own direction. \
Ask clarifying questions only when ambiguity changes write scope, acceptance, or sequencing. \
When the write boundary is clear enough for workers to start, call propose_directions with a short rationale and directions \
(name, the ONE repo each writes, reason, mandate). Only list repos each direction must WRITE; reads are free. \
Pick mandate per direction as a planning-depth hint: plan+impl for directions that need worker planning, impl-only for small or fully specified directions. \
Prefer independent directions that can proceed in parallel; put shared contract owners first only when they block others. \
The human reviews and confirms in weft; you can re-propose after more discussion.";
```

Do not change `SENTINEL_DIRECTIVES` in this task.

- [ ] **Step 4: Replace Concierge prompt body**

In `concierge_prompt`, replace the Chinese and English bodies with provider-aware policy. Chinese body:

```rust
"你是 weft 桌面端的 IM Concierge，用户从一个 IM 会话找你。weft 桌面端正在运行，真实状态都在 weft_global MCP 能力里；回答任何关于工作区、issue、待办、agent 提问的问题前，必须先用工具核实，不要凭印象作答。\n\
每条 IM 消息会带结构化 <weft:im_context>，其中包含 provider、当前会话、当前消息和 provider 能力。根据这些能力决定是否能创建或复用 issue 的原生讨论 thread/topic。\n\
当用户从 IM 创建新的 issue/task 时，使用 IM-aware 的 issue 创建能力；如果 provider 支持 issue thread/topic，默认创建并绑定，让用户进入该 issue 的原生讨论位置。\n\
当用户希望介入已有 issue、打开 issue、继续某个 task，或把话转给某个 issue lead 时，先确保该 issue 有 provider-native thread/topic，并引导用户进入那里。只有用户给出明确要转达给 lead 的内容时，才把 initial message 发送给 lead。\n\
普通状态查询、列表查询、待办查询不要创建 thread/topic。无法唯一匹配 issue 时，先列出候选并让用户选择。\n\
不要替用户决定需要桌面确认的事（scope 拍板、批准 write trigger、合并保护分支）。不要臆造 issue/工作区/ask 的细节；找不到就说没找到。不要在不可逆动作之前自行批准权限请求，除非用户在这条消息里明确同意。\n\
回复风格：简短中文，用 markdown 列表/编号；引用 issue 时带 thread_id；引用 ask 时带 ask_id。"
```

English body:

```rust
"You are weft's IM Concierge, reached by the user through one IM conversation. weft is running on the user's desktop and authoritative state lives behind weft_global capabilities; verify with tools before answering anything about workspaces, issues, pending asks, or agent questions. Never answer from memory.\n\
Each IM message includes structured <weft:im_context> with the provider, current conversation, current message, and provider capabilities. Use those capabilities to decide whether an issue can have a provider-native thread/topic.\n\
When the user creates a new issue/task from IM, use the IM-aware issue creation capability. If the provider supports issue threads/topics, default to creating and binding one so the user can continue in the issue's native discussion location.\n\
When the user wants to intervene in an existing issue, open an issue, continue a task, or relay a concrete instruction to an issue lead, first ensure that issue has a provider-native thread/topic and guide the user there. Send an initial message to the lead only when the user provided concrete text to relay.\n\
Read-only status, list, and pending-ask queries must not create threads/topics. If an issue reference is ambiguous, list candidates and ask the user to choose.\n\
Do not decide things that require the desktop: scope approval, write-trigger approval, or protected-branch merge. Do not invent workspace, issue, or ask details. Do not pre-approve irreversible permission asks unless the user explicitly consents in this message.\n\
Style: short markdown bullets or numbered lists; mention thread_id when citing an issue and ask_id when citing an ask."
```

- [ ] **Step 5: Verify prompt tests pass**

Run:

```bash
cd src-tauri && cargo test lead_prompt_is_policy_not_fixed_sequence concierge_prompt_is_provider_aware_not_feishu_scripted
```

Expected: PASS.

---

### Task 2: Make worker brief a delivery contract

**Files:**
- Modify: `src-tauri/src/brief.rs:62-95`
- Test: `src-tauri/src/brief.rs`

- [ ] **Step 1: Update brief tests first**

Replace assertions in `plan_impl_brief_carries_planning_contract` and `impl_only_brief_skips_planning` with delivery-contract expectations:

```rust
#[test]
fn plan_impl_brief_carries_delivery_contract() {
    let s = format_brief(&data());
    assert!(s.contains("Delivery contract"));
    assert!(s.contains("You may decide how much planning is needed"));
    assert!(s.contains("plan+impl"));
    assert!(!s.contains("starts in **planning**"));
}

#[test]
fn impl_only_brief_is_a_planning_depth_hint() {
    let mut d = data();
    d.mandate = "impl-only".into();
    let s = format_brief(&d);
    assert!(s.contains("impl-only"));
    assert!(s.contains("scope is considered concrete enough to build directly"));
    assert!(!s.contains("skip planning"));
}
```

- [ ] **Step 2: Run failing brief tests**

Run:

```bash
cd src-tauri && cargo test brief::tests::plan_impl_brief_carries_delivery_contract brief::tests::impl_only_brief_is_a_planning_depth_hint
```

Expected: FAIL until brief prose changes.

- [ ] **Step 3: Replace status contract prose**

In `format_brief`, replace the `if d.mandate == "impl-only" { ... } else { ... }` status-contract section with one delivery contract section:

```rust
s.push_str("\n## Delivery contract\n");
s.push_str("You own delivery for this direction in your write repo. You may decide how much planning is needed before editing. Use existing repository conventions and configured checks; do not invent toolchains. Coordinate when your changes affect other directions, and announce interface/contract changes before relying on them. Ask the human only for missing requirements, product judgment, or permission decisions. Move your task status when material progress changes. When ready for review, report what changed, what was verified, and remaining risks.\n");

let mandate = d.mandate.as_str();
if mandate == "impl-only" {
    s.push_str("\nMandate hint: **impl-only** — this scope is considered concrete enough to build directly unless you discover ambiguity that materially changes the work.\n");
} else {
    s.push_str("\nMandate hint: **plan+impl** — expect to plan your approach first unless the path is obvious, then build and verify.\n");
}
```

Keep the earlier `## Coordinate` section for tool names if needed by current agents; do not remove bus tool instructions in this task.

- [ ] **Step 4: Verify brief tests pass**

Run:

```bash
cd src-tauri && cargo test brief::tests
```

Expected: all brief tests PASS.

---

### Task 3: Add IM provider capability and context framing

**Files:**
- Modify: `src-tauri/src/im/mod.rs`
- Test: `src-tauri/src/im/mod.rs`

- [ ] **Step 1: Add failing tests for provider context**

Add tests in the existing `#[cfg(test)]` module in `src-tauri/src/im/mod.rs`:

```rust
#[test]
fn feishu_im_context_frame_contains_provider_capabilities() {
    let frame = super::format_im_user_message(
        "ou_sender",
        "oc_chat",
        "chat:oc_chat",
        Some("om_msg"),
        "创建一个 issue",
        &super::feishu_provider_capabilities(),
    );

    assert!(frame.contains("<weft:im_context>"));
    assert!(frame.contains("\"provider\":\"feishu\""));
    assert!(frame.contains("\"issue_thread\""));
    assert!(frame.contains("\"default_on_create_issue\":true"));
    assert!(frame.contains("<weft:user_message>创建一个 issue</weft:user_message>"));
    assert!(!frame.contains("feishu_chat_id="));
}
```

- [ ] **Step 2: Run failing test**

Run:

```bash
cd src-tauri && cargo test feishu_im_context_frame_contains_provider_capabilities
```

Expected: FAIL because helpers do not exist.

- [ ] **Step 3: Add capability struct and Feishu constructor**

Near `ImSettings`, add:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImProviderCapabilities {
    pub provider_id: &'static str,
    pub issue_thread_supported: bool,
    pub default_create_thread_for_new_issue: bool,
    pub can_create_thread_from_current_conversation: bool,
    pub can_reply_to_message: bool,
    pub terminology_zh: &'static str,
    pub terminology_en: &'static str,
}

pub fn feishu_provider_capabilities() -> ImProviderCapabilities {
    ImProviderCapabilities {
        provider_id: "feishu",
        issue_thread_supported: true,
        default_create_thread_for_new_issue: true,
        can_create_thread_from_current_conversation: true,
        can_reply_to_message: true,
        terminology_zh: "飞书 topic",
        terminology_en: "Feishu topic",
    }
}
```

- [ ] **Step 4: Add structured frame renderer**

Add helper:

```rust
pub fn format_im_user_message(
    sender_open_id: &str,
    chat_id: &str,
    im_thread_ref: &str,
    reply_to: Option<&str>,
    text: &str,
    caps: &ImProviderCapabilities,
) -> String {
    let ctx = serde_json::json!({
        "provider": caps.provider_id,
        "conversation": {
            "chat_id": chat_id,
            "thread_ref": im_thread_ref,
            "reply_to": reply_to,
            "sender_id": sender_open_id,
        },
        "capabilities": {
            "issue_thread": {
                "supported": caps.issue_thread_supported,
                "default_on_create_issue": caps.default_create_thread_for_new_issue,
                "can_create_from_current_conversation": caps.can_create_thread_from_current_conversation,
                "terminology": { "zh": caps.terminology_zh, "en": caps.terminology_en },
            },
            "reply": { "supported": caps.can_reply_to_message }
        }
    });
    format!(
        "<weft:im_context>{ctx}</weft:im_context>\n\n<weft:user_message>{}</weft:user_message>",
        text.trim()
    )
}
```

- [ ] **Step 5: Use renderer in `consume_free_text`**

Replace the `framed` match in `consume_free_text` with:

```rust
let framed = format_im_user_message(
    sender_open_id,
    chat_id,
    im_thread_ref,
    reply_to,
    text,
    &feishu_provider_capabilities(),
);
```

- [ ] **Step 6: Verify provider context tests pass**

Run:

```bash
cd src-tauri && cargo test feishu_im_context_frame_contains_provider_capabilities
```

Expected: PASS.

---

### Task 4: Add IM-aware global tools

**Files:**
- Modify: `src-tauri/src/bus/global.rs`
- Modify: `src-tauri/src/im/mod.rs` if shared helper is needed
- Test: `src-tauri/src/bus/global.rs`

- [ ] **Step 1: Add tests for global tool specs**

Add test in `src-tauri/src/bus/global.rs` tests:

```rust
#[test]
fn global_specs_include_im_aware_issue_tools() {
    let specs = global_specs();
    let names: Vec<String> = specs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect();
    assert!(names.contains(&"create_issue_from_im".to_string()));
    assert!(names.contains(&"ensure_issue_im_thread".to_string()));
}
```

- [ ] **Step 2: Add tests for unsupported provider degradation**

Add a pure helper test before wiring live Feishu channel calls:

```rust
#[tokio::test]
async fn create_issue_from_im_without_thread_support_creates_issue_only() {
    let db = mem_db().await;
    let asks = AskRegistry::new();
    let bus = BusRegistry::new();
    let ws = repo::create_workspace(&db, "alpha").await.unwrap();
    let args = json!({
        "workspace_id": ws.id,
        "title": "New task",
        "kind": "feature",
        "im_context": {
            "provider": "none",
            "conversation": { "chat_id": "c" },
            "capabilities": { "issue_thread": { "supported": false } }
        }
    });

    let result = call_global(&db, &asks, &bus, "create_issue_from_im", &args).await;
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("New task"));
    assert!(text.contains("thread_created"));
    assert!(text.contains("false"));
}
```

- [ ] **Step 3: Run failing global tests**

Run:

```bash
cd src-tauri && cargo test global_specs_include_im_aware_issue_tools create_issue_from_im_without_thread_support_creates_issue_only
```

Expected: FAIL until tools exist.

- [ ] **Step 4: Add IM context parser helpers**

In `global.rs`, add small helpers near tool implementations:

```rust
fn im_provider(args: &Value) -> &str {
    args.pointer("/im_context/provider").and_then(|v| v.as_str()).unwrap_or("")
}

fn im_issue_thread_supported(args: &Value) -> bool {
    args.pointer("/im_context/capabilities/issue_thread/supported")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn im_chat_id(args: &Value) -> Option<&str> {
    args.pointer("/im_context/conversation/chat_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
```

- [ ] **Step 5: Implement `create_issue_from_im` dispatch**

Add match arm before `create_issue`:

```rust
"create_issue_from_im" => {
    let Some(ws) = args.get("workspace_id").and_then(|v| v.as_i64()).map(|x| x as i32) else {
        return text_result("error: workspace_id required".into());
    };
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("").trim();
    if title.is_empty() { return text_result("error: title required".into()); }
    if kind.is_empty() { return text_result("error: kind required".into()); }
    match create_issue_from_im(db, ws, title, kind, args).await {
        Ok(v) => json_result(v),
        Err(e) => text_result(format!("error: {e}")),
    }
}
```

Implement helper:

```rust
async fn create_issue_from_im(db: &Db, ws: i32, title: &str, kind: &str, args: &Value) -> anyhow::Result<Value> {
    let issue = create_issue(db, ws, title, kind).await?;
    let thread_id = issue["id"].as_i64().unwrap_or_default() as i32;
    let provider = im_provider(args);
    let supported = im_issue_thread_supported(args);
    let mut im = json!({
        "provider": provider,
        "thread_exists": false,
        "thread_created": false,
        "thread_ref": null,
        "open_hint": "provider does not support issue thread/topic in this conversation"
    });
    if provider == "feishu" && supported {
        if let Some(chat_id) = im_chat_id(args) {
            match ensure_issue_topic(db, thread_id, chat_id).await {
                Ok(v) => {
                    im = json!({
                        "provider": provider,
                        "thread_exists": true,
                        "thread_created": v.get("created").and_then(|x| x.as_bool()).unwrap_or(false),
                        "thread_ref": v.get("im_thread_ref").cloned().unwrap_or(Value::Null),
                        "chat_id": v.get("chat_id").cloned().unwrap_or(Value::Null),
                        "open_hint": "已创建或复用飞书 topic，请进入该 topic 继续讨论"
                    });
                }
                Err(e) => {
                    im = json!({
                        "provider": provider,
                        "thread_exists": false,
                        "thread_created": false,
                        "thread_ref": null,
                        "open_hint": format!("issue created, but IM topic was not created: {e}")
                    });
                }
            }
        }
    }
    Ok(json!({ "issue": issue, "im": im }))
}
```

- [ ] **Step 6: Implement `ensure_issue_im_thread` dispatch**

Add match arm:

```rust
"ensure_issue_im_thread" => {
    let Some(tid) = args.get("thread_id").and_then(|v| v.as_i64()).map(|x| x as i32) else {
        return text_result("error: thread_id required".into());
    };
    match ensure_issue_im_thread(db, tid, args).await {
        Ok(v) => json_result(v),
        Err(e) => text_result(format!("error: {e}")),
    }
}
```

Implement helper:

```rust
async fn ensure_issue_im_thread(db: &Db, thread_id: i32, args: &Value) -> anyhow::Result<Value> {
    let issue = repo::get_thread(db, thread_id).await?.ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    let provider = im_provider(args);
    let supported = im_issue_thread_supported(args);
    let initial_message = args.get("initial_message").and_then(|v| v.as_str()).unwrap_or("").trim();
    let mut im = json!({
        "provider": provider,
        "thread_exists": false,
        "thread_created": false,
        "thread_ref": null,
        "open_hint": "provider does not support issue thread/topic in this conversation"
    });
    if let Some(route) = repo::im_route_of_thread(db, thread_id).await? {
        im = json!({
            "provider": route.channel,
            "thread_exists": true,
            "thread_created": false,
            "thread_ref": route.im_thread_ref,
            "chat_id": route.chat_id,
            "open_hint": "已有 issue topic/thread，请进入那里继续讨论"
        });
    } else if provider == "feishu" && supported {
        if let Some(chat_id) = im_chat_id(args) {
            let v = ensure_issue_topic(db, thread_id, chat_id).await?;
            im = json!({
                "provider": provider,
                "thread_exists": true,
                "thread_created": v.get("created").and_then(|x| x.as_bool()).unwrap_or(false),
                "thread_ref": v.get("im_thread_ref").cloned().unwrap_or(Value::Null),
                "chat_id": v.get("chat_id").cloned().unwrap_or(Value::Null),
                "open_hint": "已创建或复用飞书 topic，请进入该 topic 继续讨论"
            });
        }
    }
    let delivered = if !initial_message.is_empty() {
        message_lead(db, thread_id, initial_message).await.is_ok()
    } else {
        false
    };
    Ok(json!({
        "issue": { "id": issue.id, "workspace_id": issue.workspace_id, "title": issue.title, "kind": issue.kind },
        "im": im,
        "lead_message_delivered": delivered
    }))
}
```

- [ ] **Step 7: Add tool specs**

Add entries to `global_specs()`:

```rust
{
    "name": "create_issue_from_im",
    "description": "Create a Weft issue from the current IM conversation. If the provider supports issue threads/topics in this conversation, create or bind one by default so the user continues in the issue-specific discussion location.",
    "inputSchema": { "type": "object",
        "properties": { "workspace_id": i(), "title": s(), "kind": s(), "im_context": { "type": "object" } },
        "required": ["workspace_id", "title", "kind", "im_context"] }
},
{
    "name": "ensure_issue_im_thread",
    "description": "Ensure an existing issue has a provider-native IM thread/topic and guide the user there. Use when the user wants to open, enter, intervene in, or continue an issue from IM. initial_message is optional and should be set only when the user gave concrete text to relay to the lead.",
    "inputSchema": { "type": "object",
        "properties": { "thread_id": i(), "im_context": { "type": "object" }, "initial_message": s() },
        "required": ["thread_id", "im_context"] }
}
```

- [ ] **Step 8: Verify global tests pass**

Run:

```bash
cd src-tauri && cargo test global_specs_include_im_aware_issue_tools create_issue_from_im_without_thread_support_creates_issue_only
```

Expected: PASS.

---

### Task 5: Add route reuse and no-spurious-topic coverage

**Files:**
- Modify: `src-tauri/src/bus/global.rs`
- Test: `src-tauri/src/bus/global.rs`

- [ ] **Step 1: Add route reuse test**

Add:

```rust
#[tokio::test]
async fn ensure_issue_im_thread_reuses_existing_route() {
    let db = mem_db().await;
    let asks = AskRegistry::new();
    let bus = BusRegistry::new();
    let ws = repo::create_workspace(&db, "alpha").await.unwrap();
    let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude").await.unwrap();
    repo::bind_im_route(&db, issue.id, "feishu", "oc_chat", "om_root").await.unwrap();
    let args = json!({
        "thread_id": issue.id,
        "im_context": {
            "provider": "feishu",
            "conversation": { "chat_id": "oc_chat" },
            "capabilities": { "issue_thread": { "supported": true } }
        }
    });

    let result = call_global(&db, &asks, &bus, "ensure_issue_im_thread", &args).await;
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("om_root"));
    assert!(text.contains("\"thread_created\":false"));
}
```

- [ ] **Step 2: Add read-only no-route test**

Add:

```rust
#[tokio::test]
async fn read_only_global_queries_do_not_create_im_routes() {
    let db = mem_db().await;
    let asks = AskRegistry::new();
    let bus = BusRegistry::new();
    let ws = repo::create_workspace(&db, "alpha").await.unwrap();
    let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude").await.unwrap();

    let _ = call_global(&db, &asks, &bus, "list_issues", &json!({ "workspace_id": ws.id })).await;
    let _ = call_global(&db, &asks, &bus, "issue_status", &json!({ "thread_id": issue.id })).await;

    let route = repo::im_route_of_thread(&db, issue.id).await.unwrap();
    assert!(route.is_none());
}
```

- [ ] **Step 3: Run route tests**

Run:

```bash
cd src-tauri && cargo test ensure_issue_im_thread_reuses_existing_route read_only_global_queries_do_not_create_im_routes
```

Expected: PASS after Task 4 implementation.

---

### Task 6: Run final verification

**Files:**
- Affected Rust files from Tasks 1-5

- [ ] **Step 1: Format Rust**

Run:

```bash
cd src-tauri && cargo fmt
```

Expected: no errors.

- [ ] **Step 2: Run focused Rust tests**

Run:

```bash
cd src-tauri && cargo test brief::tests global::tests lead_chat::commands::tests im::tests
```

Expected: PASS. If the exact module path for any test group differs, run the specific test names added in this plan plus the full suite in Step 3.

- [ ] **Step 3: Run full Rust tests**

Run:

```bash
cd src-tauri && cargo test
```

Expected: all tests PASS.

- [ ] **Step 4: Run frontend build**

Run:

```bash
pnpm build
```

Expected: TypeScript and Vite build PASS.

- [ ] **Step 5: Check whitespace**

Run:

```bash
git diff --check
```

Expected: no whitespace errors.

---

## Baseline Already Captured

The isolated worktree was created at `.worktrees/agent-autonomy-im-routing` on branch `feat/agent-autonomy-im-routing` from the current branch. Baseline verification before implementation:

```bash
pnpm build
# PASS

cd src-tauri && cargo test
# 325 passed
```
