<div align="center">

<img src="public/weft-logo.svg" alt="Weft" width="240" />

### Local-first delivery hub for coding agents

Give Weft one task. It coordinates your own Claude Code, Codex, and OpenCode across multiple repositories until the work becomes reviewable, mergeable code.

**local-first · no server · automation-first**

[简体中文](README.zh-CN.md) · **English**

<sub>Tauri v2 · React 19 · Rust · SQLite</sub>

</div>

---

> **Weft** is a local-first desktop delivery hub for multi-repo software work.
> You describe a **Task**; Weft plans the scope, decides which repositories need
> changes, starts native coding-agent CLIs, coordinates their work, and verifies
> the result. **You supervise the flow and handle exceptions. You are not a
> required checkpoint for every step.**
>
> Today, Weft drives each affected repository toward a clean Pull Request. The
> longer-term direction is to keep going after the PR: merge, then deploy through
> the environments your repositories already use, from staging to production.

Weft is not a terminal emulator, and it is not a dashboard for watching agents
scroll. It is the local workspace and automation layer where agents turn one
intent into coordinated delivery across repositories.

<p align="center">
  <img src="assets/screenshots/board-workspace.png" alt="Weft workspace board" width="900" />
  <br><sub><i>The workspace board: each thread is a live card showing what is running, what failed, and what needs you.</i></sub>
</p>

---

## How It Works

A workspace is a logical set of repository references. One **Task** is decomposed
into parallel **directions**. In the current implementation, each direction owns
exactly one write repository and gets one isolated git worktree; reads are free
and do not need to be declared. The directions converge toward a reviewable
worktree diff with executable checks. Opening PRs is the next delivery boundary;
the longer roadmap extends that flow through merge and environment-aware
deployment.

<p align="center">
  <img src="assets/readme/generated/flow.png" alt="Conceptual flow from one task to coordinated repository work and a pull request" width="940" />
</p>

---

## Why Weft Exists

Most agent tools are built around a chat session or a single repository. Weft is
built around **cross-repo scope decomposition**: turning one task into "these
repositories, this split of work, this order, and this agent for each part."

| | Most agent tools | **Weft** |
|---|---|---|
| **Unit of work** | A chat or one repository | One **Task** spanning many repositories |
| **Scope** | You split the work by hand | The **Lead** derives scope from a live repository map |
| **Isolation** | One working tree | One **git worktree** per write repository, created only when needed |
| **Human role** | Drive each step | **Supervise**; intervene only on exceptions |
| **Quality gate** | Human judgment | **Executable verification**: lint · type · test · contract |
| **Delivery target** | Open-ended output | PRs today; merge and deploy are the roadmap |
| **Agent CLIs** | Wrapped or proxied | **Native CLIs** with hooks, skills, and permissions intact |

<p align="center">
  <img src="assets/screenshots/repo-graph.png" alt="Cross-repo dependency map" width="900" />
  <br><sub><i>The curator's cross-repo dependency map: repository roles, stacks, and relationships such as "core · N dependents". This is the input for scope decomposition.</i></sub>
</p>

---

## Core Model

Weft is organized around four nested layers. Sessions carry an explicit role, so
planning, coordination, and implementation stay separate.

<p align="center">
  <img src="assets/readme/generated/model.png" alt="Conceptual model of workspaces, threads, directions, sessions, and agent roles" width="880" />
</p>

<p align="center">
  <img src="assets/screenshots/lead.png" alt="Lead conversation home" width="900" />
  <br><sub><i>Home is the Lead conversation. The Lead reads across repositories, plans the work, and drives workers. Board / Lead tabs switch between the live board and the coordinating conversation.</i></sub>
</p>

- **Curator** profiles each repository with its role, interfaces, and stack, then
  builds the cross-repo dependency map used for decomposition.
- **Lead** is the main conversation and control tower. It reads repositories,
  derives scope, starts workers, and coordinates them over a thread bus. **It
  never writes code and never consumes raw worker transcripts**; workers report
  structured summaries and diff stats.
- **Worker** executes one direction in its own worktree from a structured
  **brief** containing scope, interface contracts, and acceptance criteria.

---

## Board As Trust Surface

Because Weft does not put a human gate in front of every step, the board is not a
manual to-do list. It is a live projection of agent state, git state, and check
state. Cards move through the lifecycle automatically; you act on the exceptions
that surface.

The board has two levels:

- **Workspace board**: one card per **thread**, giving a portfolio view of the
  workspace. Cards show task kind, direction count, running work, failing checks,
  and whether anything **Needs you**.
- **Thread board**: one card per **direction / task**, focused on a single line
  of work. The **Board ↔ Lead** tabs switch between the cards and the Lead
  conversation.

<p align="center">
  <img src="assets/readme/generated/board.png" alt="Conceptual trust board with running work, review state, and exceptions" width="880" />
</p>

<p align="center">
  <img src="assets/screenshots/board-thread.png" alt="Weft thread board" width="900" />
  <br><sub><i>A thread board: directions move through the lifecycle, each tagged with its tool and live status. An open ask or failing check moves a card into <b>Needs you</b>.</i></sub>
</p>

- **Needs you is the exception lane.** Any open permission request or failing
  check is surfaced there, regardless of the task's stored status. It is
  aggregated across threads and shown at the top of every view.
- **Cards carry evidence.** Running sessions, failing checks, and verification
  provenance are expandable. Green should be trustworthy; red should be
  actionable.
- **The human acts, not babysits.** The main verbs are Approve, Answer, Open,
  and Review. Manual drag-to-status remains available when you want to override
  what the agents inferred.

---

## Product Principles

1. **Automation is the direction.** The default path is autonomous: task in,
   deliverable code out. Interfaces are built for supervising the flow, not
   pushing every step.
2. **Humans handle exceptions, not the assembly line.** Weft adds no approval
   gate of its own. Blocking prompts come from the native tools or from a
   configurable irreversible-action boundary such as protected-branch merge or
   production deployment.
3. **Run native CLIs, render the conversation yourself.** Weft starts `claude`,
   `codex`, and `opencode` as normal binaries under the user's own
   configuration, preserving hooks, skills, and permissions. Each CLI is driven
   headless through its structured JSON stream, and Weft renders its own
   conversation UI; any session can be taken over in your own terminal at any
   time.
4. **Keep cross-repo wiring temporary.** Sibling repositories are mounted
   read-only through launch arguments such as `--add-dir`; Weft does not write
   that wiring into a canonical repository's config.
5. **Hide mechanisms, show decisions.** Worktrees, headless agent processes,
   the MCP bus, and sidecars live under **Inspect**. Task, scope, branch, PR,
   diff, tool choice, and brief stay first-class.
6. **Bilingual from the start.** UI text and agent-output language are both
   language-aware. Internal state enums stay English; code and identifiers stay
   English.

---

## Architecture

<p align="center">
  <img src="assets/readme/generated/architecture.png" alt="Conceptual local-first architecture for Weft" width="900" />
</p>

**Locked stack**: Tauri v2 (Rust + React / TypeScript / Vite) · headless chat
engine over the CLIs' native JSON streams · SQLite (sea-orm) · system
`git worktree` · `react-i18next`.

---

## Getting Started

> **Prerequisites:** [Node.js](https://nodejs.org) 18+, the
> [Rust toolchain](https://rustup.rs), and the platform dependencies for
> [Tauri v2](https://v2.tauri.app/start/prerequisites/). To drive agents, install
> one or more of the [Claude Code](https://claude.com/claude-code),
> [Codex](https://github.com/openai/codex), or
> [OpenCode](https://opencode.ai) CLIs.

```bash
# install frontend dependencies
npm install

# run the desktop app in development mode (Vite + Tauri)
npm run tauri dev

# build a release bundle
npm run tauri build
```

Frontend-only iteration without the Rust shell:

```bash
npm run dev        # Vite dev server
npm run build      # type-check + production build
```

Backend tests:

```bash
cd src-tauri && cargo test
```

---

## Project Layout

```text
src/                  React frontend
  board/              two-level board, repo graph, write-scope review, Needs you
  session/            chat timeline, composer, observe and diff views
  nav/  components/    workspace nav, dialogs, UI primitives, Inspect
  i18n/               en / zh resources and runtime switching
src-tauri/src/        Rust backend
  lead_chat/          headless chat engine: claude stream-json (resident),
                      codex exec --json · opencode run --format json (per turn)
  sidecar.rs          native transcript readers → normalized observe events
  ask.rs              Ask Bridge: permission asks → Needs-you cards → decisions back
  planner.rs          Task → proposed directions, one write repo per direction
  curator.rs          deterministic repo profiles + dependency graph
  coordinator.rs      bus wakeups → invisible queued nudges
  brief.rs            worker brief assembled from task, repo graph, mandate
  check.rs            inferred lint/type/build/test/contract checks
  config.rs           effective Claude skills/rules preview
  bus/                thread bus (MCP / axum server) + coordinator nudges
  materialize.rs      confirmed write direction → namespaced git worktree
  store/              SQLite schema, migrations, repositories
ARCHITECTURE.md       full design and feasibility study
PRODUCT.md  DESIGN.md product thesis and visual system
```

---

## Status

Weft is in **active development**. The current codebase implements the core
local app shell and a substantial vertical slice:

- Tauri v2 + React 19 + SQLite via SeaORM migrations.
- Workspace / repo / thread / direction / worktree / session / lead-message
  persistence, including repo clone/create/add and cascade cleanup.
- Deterministic repo profiling and a cross-repo dependency graph from manifests.
- A lead conversation backed by Claude stream-json plus planner MCP tools.
- Chat-mode workers for Claude, Codex, and OpenCode through one chat engine:
  Claude is resident; Codex and OpenCode are per-turn processes.
- Worker resume, interrupt, terminal takeover commands, Codex app links, file
  attachments, image handling, slash-command discovery, streaming deltas, and
  transient activity rows.
- Planner proposals where each direction declares one write repo with a reason
  and mandate (`plan+impl` or `impl-only`); pending write declarations surface in
  Needs-you and materialize only when approved or confirmed.
- Ask Bridge for tool permissions through generated hooks/plugins, with
  Allow / Deny / Always / Full plus global Dangerous mode.
- Thread bus over a local MCP/HTTP server, human asks, shared state, interface
  broadcasts, and coordinator wakeups that queue invisible nudges.
- Sidecar transcript readers for Claude jsonl, Codex rollout jsonl, and
  OpenCode SQLite, normalized into Observe events.
- Inferred verification rungs for Node, Rust, Go, Python, and buf contracts;
  auto-checks run when workers settle, and review runs as the configured skill
  inside the worker conversation.
- Two-level board, repo map, Lead tab, worker session view, Observe/Diff panels,
  Needs-you surface, settings, onboarding, command palette, light/dark theme, and
  zh/en UI plus agent-output language preference.
- Runaway guardrails: wall-clock and idle caps force-stop stuck turns and raise
  a Needs-you question; defaults are configurable in Settings and via `WEFT_*`
  environment variables.

Still not implemented as product behavior: automated PR creation, protected
branch merge, staging/production deployment orchestration, team marketplace
sync, a long-lived semantic curator agent, and full CI/CD observation.

**Roadmap boundary.** Current code reaches reviewable local worktree diffs with
pre-PR checks. The product boundary is Task → PR next; the longer-term target is
to continue through auto-merge and environment-aware deployment, so "done" means
shipped code rather than an open PR.

For deeper context, see [`ARCHITECTURE.md`](ARCHITECTURE.md), [`PRODUCT.md`](PRODUCT.md),
and [`DESIGN.md`](DESIGN.md).

---

<div align="center">
<sub>Composed, exact, quietly alive. — Weft</sub>
</div>
