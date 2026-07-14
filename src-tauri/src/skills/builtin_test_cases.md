<!-- weft-builtin -->
---
name: weft-derive-test-cases
description: Use when deriving or revising an issue's test cases (the <weft:test_cases> document) — a draft → enrich → adversarial review → clarify → finalize workflow with hard quality gates for decidable, user-observable cases.
---

# Deriving an issue's test cases

Flow: **draft outline → enrich from code → adversarial review → clarify with the human → finalize as `<weft:test_cases>`**. The sentinel is the document's only home — weft stores it, renders it as a mindmap, and lets the human edit it; never duplicate the tree in prose.

## Non-negotiables

- A case is 「测什么 → 怎么操作 → 期望结果」, never a paraphrase of the requirement text.
- The tree speaks USER language: interactions and observable outcomes only. No interfaces, fields, SDKs, DB tables, logs, or analytics events in the tree — translate any code finding into what a user would see or do.
- Every leaf must be DECIDABLE: a concrete action plus an observable expected result. Vague leaves like 「正常展示」「符合预期」「功能可用」 are banned.

## Pre-finalize gate

Before emitting `<weft:test_cases>`, ALL of these hold:

- The requirement conversation has converged (you understand goal, boundaries, acceptance).
- User-provided designs / prototypes / screenshots have been checked, when given.
- Existing code has been scouted when the issue reuses, extends, or trims existing behavior — you may read any repo in the workspace.
- An adversarial review has run in an independent context (see step 6).
- Every open question that affects behavior judgement is resolved with the human — ask, never guess.

## Workflow

1. **Profile the feature.** Switches/config, roles, special objects, single- vs multi-end, cross-end sync — the profile decides the tree's shape.
2. **Platform strategy.** One document by default. Split `pc端` / `移动端` under a NODE only where the gesture differs (hover vs long-press); only a fully divergent business flow justifies separate trees.
3. **First-level skeleton**, only for modules that actually exist (never pad): 入口 → 开关/配置变更 → 核心功能流 → 特殊场景 → 异常与边界 → 兼容性.
4. **Fill each module.** Every core operation passes three coverages:
   - 功能路径: forward / reverse / boundary.
   - UI 交互路径: disabled states, click feedback, panel collapse, hover, long-press, soft keyboard, scrolling, focus.
   - 横切维度: permissions, multi-perspective views, copywriting, multi-end sync, concurrency, special objects, old-vs-new versions.
5. **Enrich from existing code** when the issue touches existing behavior: real entries, hidden entries, compatibility risks — each finding translated into a user-observable case (no class/interface/field names in the tree).
6. **Adversarial review in an independent context.** Use a sub-agent when your CLI supports one; otherwise run a separate self-review pass that assumes only the requirement is known. Attack three angles: missed or wrong cases vs the requirement; UI interaction chains left incomplete; forward/reverse/boundary gaps. Fold findings into the tree or the open-questions list.
7. **Draft in conversation FIRST.** Post the outline and the open questions as plain chat text — not the sentinel. Any question that affects behavior judgement MUST be asked; guessing is not an option. Small, fully-specified issues may collapse steps 1–7 into a brief pass, but the lints in step 8 still apply.
8. **Finalize.** After clarifications, run both lints, then emit `<weft:test_cases>` (raw markdown tree; re-emitting replaces the whole document):
   - Decidability lint: every leaf names an action and an observable result.
   - Tech-detail lint: no APIs, fields, SDKs, DB, logs, or analytics anywhere in the tree.

After the human edits the document in weft you receive `<weft:test_cases_updated>` — carry their version forward and only re-emit when you are deliberately changing it.
