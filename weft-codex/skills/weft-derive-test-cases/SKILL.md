---
name: weft-derive-test-cases
version: 1
description: Use when deriving or revising an issue's test cases in weft-codex — a draft → enrich → independent adversarial review → clarify → finalize workflow, published as a versioned `test_cases` artifact through the weft MCP tools. Also use when the human edits the document and asks you to carry their version forward.
---

# Deriving an issue's test cases

Flow: **draft outline → enrich from code → independent adversarial review → clarify with the human → publish as a `test_cases` artifact**.

The artifact is the document's only home. It belongs to the issue, not to your
thread, so every fork of the lead sees the same one — and the human edits it
directly.

**Publish early and revise.** The moment you have a tree worth showing, write it
as a `draft` artifact; do not hold it in chat until the end. A derivation pass
is long and a turn can end mid-way, and a tree that only ever existed in prose
leaves the human with nothing to open and nothing to edit. Summarise in chat by
all means — but the document itself lives in the artifact from the first draft
onward.

## Where the document lives

Publish through the weft MCP tools, never by printing a marker for someone to
parse:

- `artifact_list` — does a `test_cases` artifact already exist for this issue?
- `artifact_read` — full content **and the current revision**.
- `artifact_write` — create (omit `id`) or revise (`id` + `expected_revision`).
- `artifact_status` — mark `ready` once the pre-finalize gate passes.

**Always read immediately before writing.** The human may have edited the
document since you last saw it, and a write carrying a stale `expected_revision`
is refused. When that happens you get back `revision_conflict` with the revision
that won: read it, re-apply your change on top of *their* version, and write
again. Never retry the same payload, and never overwrite an edit you have not
looked at.

Use `format: "markdown_tree"` — weft renders it as an editable mindmap.

## When not to do this

- The issue is trivial and fully specified — say so and move on.
- The human explicitly asked you to skip it — respect that without arguing.
- The human explicitly asked for it — do it even if the issue looks small.

## Non-negotiables

- A case is 「测什么 → 怎么操作 → 期望结果」, never a paraphrase of the
  requirement text.
- The tree speaks USER language: interactions and observable outcomes only.
  Translate every code finding into what a user would do or see.
- Every leaf is DECIDABLE: a concrete action plus an observable result.

## Workflow

1. **Profile the feature.** Switches/config, roles, special objects, single- vs
   multi-end, cross-end sync — the profile decides the tree's shape.
2. **Platform strategy.** One document by default. Split `pc端` / `移动端` under
   a node only where the gesture genuinely differs (hover vs long-press); only a
   fully divergent business flow justifies separate trees.
3. **First-level skeleton**, only for modules that actually exist — never pad:
   入口 → 开关/配置变更 → 核心功能流 → 特殊场景 → 异常与边界 → 兼容性.
4. **Fill each module.** Every core operation passes three coverages:
   - 功能路径: forward / reverse / boundary.
   - UI 交互路径: disabled states, click feedback, panel collapse, hover,
     long-press, soft keyboard, scrolling, focus.
   - 横切维度: permissions, multi-perspective views, copywriting, multi-end
     sync, concurrency, special objects, old-vs-new versions.
5. **Enrich from existing code** when the issue touches existing behaviour: real
   entries, hidden entries, compatibility risks. Every finding must be
   translated into a user-observable case before it enters the tree.
6. **Publish the draft as soon as a tree exists — before the review.**
   `artifact_write` it with status left at `draft`. Do not wait for the review
   or for the human's answers.

   This is not optional sequencing. A derivation pass is long, and a turn can
   end at any point in it; if the tree lives only in chat until the end, an
   interrupted turn leaves the human with nothing to look at and nothing to
   edit. Publishing first also means the human can start editing while the
   review runs. Everything after this point is a revision, not a first write.
7. **Adversarial review — in a genuinely separate context.** See below. Fold
   the findings in with `artifact_write` (`id` + `expected_revision`).
8. **Clarify.** Post the open questions as chat. Any question that affects a
   behaviour judgement MUST be asked; guessing is not an option. Fold the
   answers in as another revision.
9. **Finalize.** Run both lints, apply any last revision, then
   `artifact_status(status: "ready")`.

## Step 6: the adversarial review must be independent

This is the step most easily faked. Re-reading your own tree and nodding at it
is not a review — you already believe it, so you will find nothing.

Independent means the reviewer starts from the requirement alone and has **not**
seen your reasoning:

- Preferred: spawn a sub-agent and give it only the requirement plus the tree.
- Otherwise: start a fresh context. State the requirement, paste the tree, and
  review without re-deriving — do not re-explain why you made each choice.

Attack three angles, and report findings even when there are none to report:

1. Cases missing or wrong versus the requirement.
2. UI interaction chains left incomplete.
3. Forward / reverse / boundary gaps.

Fold every finding into the tree or into the open-questions list.

## The two lints

Both run before `artifact_write`. A tree that fails either is not ready.

### Decidability lint

Every leaf names an action **and** an observable result. If two testers could
disagree about whether it passed, it fails.

| | |
|---|---|
| ✅ | 点击「保存」后,列表首行出现新建的草稿,标题为刚输入的文本 |
| ✅ | 未填写标题时点击「保存」,「保存」按钮保持禁用且不发起请求 |
| ❌ | 正常展示 |
| ❌ | 功能可用 |
| ❌ | 符合预期 |
| ❌ | 保存成功 |

The last one is the subtle failure: "保存成功" names an outcome but nothing
observable — success according to what? The fix is to say what the user sees.

### Tech-detail lint

No APIs, fields, SDKs, database tables, logs, or analytics events anywhere in
the tree. A code finding is fine as *input*; it must be translated before it
becomes a leaf.

| | |
|---|---|
| ✅ | 断网时点击「提交」,出现「网络异常,请重试」提示,已填内容不丢失 |
| ✅ | 从旧版本升级后打开列表,此前收藏的条目仍在且顺序不变 |
| ❌ | 调用 `POST /api/orders` 返回 500 时展示错误 |
| ❌ | `order_status` 字段为 `PENDING` 时按钮置灰 |
| ❌ | 上报 `click_submit` 埋点 |

Same shape each time: the ❌ rows name something only a developer can see. Ask
"could the tester observe this without opening the code or a console?" — if not,
rewrite it as what appears on screen.

## After the human edits

The human edits the artifact directly in weft, which bumps its revision. When
you next touch it, `artifact_read` first and carry their version forward. Only
re-publish when you are deliberately changing something, and say what you
changed and why — they are looking at the same document you are.
