<div align="center">
  <img src="public/weft-logo.svg" alt="Weft" width="220" />

### Local multi-repo delivery orchestration for your coding agents

Weft is a local multi-repo delivery orchestrator. Give it a requirement, and it
coordinates your own Claude Code, Codex, and OpenCode across repositories to
carry the work from intent toward implementation, merge, and release.

<sub>Tauri v2 · React 19 · Rust · SQLite · native coding-agent CLIs</sub>

[中文说明](README.zh-CN.md)
</div>

<p align="center">
  <img src="assets/readme/weft-overview.png" alt="Weft overview: repositories feed a lead workspace, scoped workers produce checked review diffs" width="940" />
</p>

## The 30-second version

Weft is not a terminal grid and not a hosted agent runner. It is the local
orchestration layer between a product requirement and the native coding agents,
repositories, branches, checks, and release paths you already use.

The north-star loop:

```text
Requirement → repo map → scoped agent lanes → repo-native branches → implementation → PR / merge / release
```

### 1. Cross-repo scope decomposition

You describe a feature, bugfix, refactor, or spike. The lead agent uses the
workspace repo map to decide which repositories need writes, why each write lane
exists, and which worker should take it. Reads stay free; only writes are scoped,
approved, materialized, and tracked.

### 2. Respect user origin

Weft drives the native tools you already use: Claude Code, Codex, and OpenCode.
It does not replace their auth, hooks, approvals, sandbox rules, skills, or
session identity. Permission asks are mirrored into Weft; they are not bypassed.
Terminal takeover remains one step away when you want the original CLI surface.

### 3. Respect repo origin

Weft does not impose an internal branch scheme on your repository. New work is
materialized under the target repo:

```text
<repo>/.worktrees/weft/<branch-name>
```

Branch names follow that repo's observed style: `feat/*` vs `feature/*`,
`fix/*` vs `bugfix/*`, with numeric suffixes only when needed. Weft keeps its
own routing in local state; your git history keeps looking like your git
history.

### 4. Bring team playbooks, keep personal taste

Teams can import git-hosted skill sources, sync them locally, and enable each
skill globally or only for a workspace. Your personal native-CLI skills still
exist, repo-owned skills can win by name, and Weft shows the effective skills
and rules before a session runs. The same model is the path for workspace rules:
shared defaults, selective opt-in, repo rules last.

## What it feels like

<p align="center">
  <img src="assets/diagrams/flow-en.svg" alt="Task to scoped sub-tasks to verified worktree diffs" width="940" />
</p>

1. Add existing repositories to a workspace.
2. Start an issue and describe the goal to the lead agent.
3. Review the proposed write lanes: repository, reason, tool, and mandate.
4. Approve the lanes that should become worktrees.
5. Workers run headless native CLI sessions and stream back into Weft.
6. You answer real blockers, inspect diffs, and run checks before PR.

The human handles exceptions, not the assembly line.

## Product model

- **Workspace**: a logical set of repo references, profiles, rules, and tools.
- **Issue**: one user-facing work line for a feature, bugfix, refactor, or spike.
- **Sub-task**: one scoped worker lane with one write repository today.
- **Session**: one native agent run attached to a worktree.

Internally the store still uses `thread` for Issues and `direction` for
Sub-tasks. User-facing docs and UI use **Issue** and **Sub-task**.

## Product surfaces

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

## IM remote control

<p align="center">
  <img src="assets/diagrams/im-en.svg" alt="IM remote control: Feishu cards mirror permission asks and agent questions" width="940" />
</p>

Workers can mirror permission asks and agent questions to Feishu/Lark as
interactive cards. Replying on mobile resolves the same underlying ask the
desktop UI would resolve, and both surfaces patch to the same final state.

The bridge currently covers:

- Permission asks and agent questions.
- Issue-to-Feishu topic routes for lead messages; bind a topic by sending
  `/bind <issue-id>` from that Feishu topic.
- Concierge-style direct chat backed by the `weft_global` MCP tools.
- Online resync summaries for pending Needs-you items.

Binding is conservative: the first private-chat sender can become owner, group
messages cannot bind ownership, and DB errors fail closed.

## Current capabilities

- Workspace repo add/clone/create flows with deterministic repo profiles.
- Repo map powered scope proposals: the lead explains which repo each lane writes and why.
- Repo-native worktrees and branch names that follow each target repo's style.
- Claude lead sessions with planner MCP and write-scope review.
- Lead action cards for adding, cloning, or creating repos from the conversation.
- Worker sessions for Claude Code, Codex, and OpenCode.
- Weft-owned chat timeline with queueing, interrupt, resume, slash commands, and attachments.
- Ask Bridge for tool permission requests: Allow, Always, Full, and Deny.
- Skill source manager with git-backed sync, personal skill preservation, and global/workspace enablement.
- Effective config preview for the skills and rules that apply to each repo, including their layer and overrides.
- Sidecar observation for Claude jsonl, Codex rollout jsonl, and OpenCode SQLite.
- Diff and pre-PR check surfaces from materialized worktrees.
- Rename and cascade-delete for workspaces, issues, and sub-tasks.
- English and Chinese UI.

Not yet productized: automatic PR creation, protected-branch merge orchestration,
CI/CD observation, deployment orchestration, workspace rule packs, team
marketplace sync, and the long-running semantic Curator.

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
