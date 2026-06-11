<div align="center">
  <img src="public/weft-logo.svg" alt="Weft" width="220" />

### Local-first delivery hub for coding agents

Give Weft a feature, bugfix, or refactor. A lead agent turns it into scoped
worker lanes, Weft materializes each approved lane as an isolated `git worktree`,
and Claude Code, Codex, or OpenCode drive the work until there is a diff you can
review.

<sub>Tauri v2 · React 19 · Rust · SQLite · native coding-agent CLIs</sub>

[中文说明](README.zh-CN.md)
</div>

<p align="center">
  <img src="assets/readme/weft-overview.png" alt="Weft overview: repositories feed a lead workspace, scoped workers produce checked review diffs" width="940" />
</p>

## Why Weft

Coding agents are strongest when they get tight scope, real repository context,
and a clear handoff back to a human. Weft keeps that loop local:

- Your source stays on your machine.
- Workers run through the native CLIs you already use and authenticate.
- Every worker gets an explicit write repository and its own worktree.
- The product UI shows the plan, live sessions, permission asks, diffs, and
  pre-PR checks without embedding a terminal as the main experience.

The core product model is small:

- **Workspace**: a logical set of repositories, profiles, rules, and tools.
- **Issue**: one user-facing work line for a feature, bugfix, refactor, or spike.
- **Sub-task**: one scoped worker lane, currently with one write repository.
- **Session**: one native agent run attached to a worktree.

Internally the store still uses `thread` for Issues and `direction` for
Sub-tasks. User-facing docs and UI use **Issue** and **Sub-task**.

## Workflow

<p align="center">
  <img src="assets/diagrams/flow-en.svg" alt="Task to scoped sub-tasks to verified worktree diffs" width="940" />
</p>

1. Add, clone, or create repositories in a workspace.
2. Start an issue and discuss the goal with the lead agent.
3. The lead proposes sub-tasks with write scope, tool choice, reason, and mandate.
4. You approve the write declarations that should become worktrees.
5. Workers run in headless Claude/Codex/OpenCode sessions and stream into Weft.
6. You observe progress, answer asks, inspect diffs, and run checks before PR.

## Product Surfaces

| Workspace board | Issue board |
|---|---|
| <img src="assets/screenshots/board-workspace.png" alt="Workspace board" /> | <img src="assets/screenshots/board-issue.png" alt="Issue board" /> |

| Lead conversation | Repository map |
|---|---|
| <img src="assets/screenshots/lead.png" alt="Lead conversation" /> | <img src="assets/screenshots/repo-graph.png" alt="Repository dependency map" /> |

## Architecture

<p align="center">
  <img src="assets/diagrams/arch-en.svg" alt="Weft local-first architecture" width="940" />
</p>

The Rust backend owns the local SQLite store, git worktree lifecycle, headless
agent processes, Ask Bridge, local MCP bus, IM bridge, skill sources, and sidecar
observation. The React frontend renders the workspace board, issue board, lead
conversation, worker sessions, observe/diff views, settings, and Needs-you queue.

<p align="center">
  <img src="assets/diagrams/model-en.svg" alt="Workspace, issue, sub-task, session model" width="860" />
</p>

## IM Remote Control

<p align="center">
  <img src="assets/diagrams/im-en.svg" alt="IM remote control: Feishu cards mirror permission asks and agent questions" width="940" />
</p>

Workers can mirror permission asks and agent questions to Feishu/Lark as
interactive cards. Replying on mobile resolves the same underlying ask the
desktop UI would resolve, and both surfaces patch to the same final state.

The bridge currently covers:

- Permission asks and agent questions.
- Issue-to-Feishu thread routes for lead messages; bind a topic by sending
  `/bind <issue-id>` from that Feishu topic.
- Concierge-style direct chat backed by the `weft_global` MCP tools.
- Online resync summaries for pending Needs-you items.

Binding is conservative: the first private-chat sender can become owner, group
messages cannot bind ownership, and DB errors fail closed.

## Current Capabilities

- Workspace repo add/clone/create flows with deterministic repo profiles.
- Claude lead sessions with planner MCP and write-scope review.
- Lead action cards for adding, cloning, or creating repos from the conversation.
- Worker sessions for Claude Code, Codex, and OpenCode.
- Weft-owned chat timeline with queueing, interrupt, resume, slash commands, and attachments.
- Ask Bridge for tool permission requests: Allow, Always, Full, and Deny.
- Skill source manager with git-backed sync and global/workspace enablement.
- Sidecar observation for Claude jsonl, Codex rollout jsonl, and OpenCode SQLite.
- Diff and pre-PR check surfaces from materialized worktrees.
- Rename and cascade-delete for workspaces, issues, and sub-tasks.
- English and Chinese UI.

Not yet productized: automatic PR creation, protected-branch merge orchestration,
CI/CD observation, deployment orchestration, team marketplace sync, and the
long-running semantic Curator.

## Development

```bash
npm install
npm run dev          # Vite frontend
npm run build        # TypeScript check + production frontend bundle
npm run tauri dev    # full desktop app
npm run tauri build  # release app bundle
cd src-tauri && cargo test
git diff --check
```

## Project Layout

```text
src/
  board/                workspace and issue boards
  session/              chat, observe, diff, permissions
    blocks/             chat-timeline rich blocks
    useRepoActions.ts   add / clone / create repo from lead action cards
  components/           shared React UI
  i18n/                 English and Chinese strings
src-tauri/src/
  lead_chat/            headless agent session engine
    sentinels.rs        parse <weft:action_card> / <weft:list_repos/> markers
    repo_state.rs       <repo_state> snapshot injected into the lead prompt
  im/                   IM bridge (Channel trait + Feishu adapter, ws + cards)
  store/                SQLite/SeaORM entities and migrations
  bus/                  local MCP/thread bus + human-ask notifier
  ask.rs                permission Ask registry (desktop + IM mirrored)
  git.rs                repository and worktree operations
  materialize.rs
assets/
  screenshots/          README screenshots
  diagrams/             architecture and model diagrams
  readme/               generated README overview art
```

## Design Constraints

Weft drives native CLIs through structured, headless interfaces and renders its
own UI. Do not add embedded terminal/TUI dependencies for normal chat surfaces.
Terminal takeover remains an escape hatch for users who want the original CLI.
