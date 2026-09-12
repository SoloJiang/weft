<div align="center">

# weft-codex

### A Codex-native workspace for multi-repository delivery

weft-codex turns one product issue into coordinated Codex threads across the
repositories that make up your product. A lead understands the workspace and
creates repository-scoped tasks; workers execute in isolated worktrees; Kanban
and a durable thread bus keep progress and handoffs visible inside the
official Codex Desktop app.

<sub>Official Codex Desktop · React · Rust · SQLite · Codex app-server</sub>

[中文说明](README.zh-CN.md)

</div>

## The 30-second version

Codex is excellent at working inside a thread. weft-codex adds the product layer
that coordinates an issue across threads and repositories.

```text
Workspace → Issue → Lead thread → repository-scoped tasks
                                  ↓
                         Worker threads + worktrees
                                  ↓
                         Thread Bus + Kanban
```

You create a Workspace, add the repositories that belong together, and describe
an Issue. The lead reads the repository map, breaks the Issue into tasks, and
starts the workers. You stay in Codex while weft-codex owns coordination,
worktree isolation, status, communication, and restart recovery.

There is no second chat client and no manual task-creation form. Codex owns the
conversations; weft-codex owns the delivery context around them.

## The product experience

1. **Create a Workspace.** Add one or more local Git repositories. weft-codex
   normalizes their roots, detects base branches and remotes, profiles each
   repository, and maps cross-repository relationships.
2. **Describe an Issue.** The Issue is the user-visible outcome and the unit shown
   on the Workspace board.
3. **Work with the lead.** A native Codex thread receives the Issue and repository
   context. The lead decides how to split the work and creates tasks itself.
4. **Let workers execute.** Every task gets a native Codex worker thread and an
   isolated repository worktree.
5. **Keep agents coordinated.** The lead and workers communicate through a durable,
   issue-scoped Thread Bus without sharing one transcript.
6. **Follow the delivery.** Kanban shows queued, working, review, and done
   states while native Codex threads remain available for direct conversation.
7. **Resume after interruption.** Thread identities, task state, worktrees, and
   pending bus messages survive daemon restarts.

## The product model

| Product object | What it means |
|---|---|
| **Workspace** | A long-lived product context containing related repositories and Issues. |
| **Repository profile** | Automatically collected repository identity, base branch, structure, and relationship context used by the lead. |
| **Issue** | One user-visible problem or outcome. It owns the lead, tasks, activity, and aggregate progress. |
| **Lead** | The native Codex thread that understands the Issue, decomposes it, creates tasks, and coordinates workers. |
| **Task** | One repository-scoped unit of work created by the lead. Internal implementation names never appear in the user experience. |
| **Worker** | A native Codex thread executing one task in an isolated Git worktree. |
| **Thread Bus** | Durable, issue-scoped communication between the lead and workers. |

## Why weft-codex

### It feels like Codex

The visible application is the official Codex Desktop app. Lead and worker
conversations are native Codex threads, Weft Mode uses Codex theme and locale
context, and opening a task switches directly to its native conversation. The
Host has no window, Dock icon, or parallel chat surface.

### The Issue stays above the threads

Threads are execution surfaces, not the product model. Workspace context, task
ownership, repository boundaries, progress, and handoffs remain attached to the
Issue even when individual threads restart or are replaced.

### Multi-repository work is native

A Workspace can contain several repositories. The lead receives their profiles
and relationships, then creates the smallest useful set of repository-scoped
tasks. Single-repository Issues use the same flow without extra ceremony.

### Lead and workers can collaborate

Workers do not need to share one context window. The Thread Bus gives every
participant a durable inbox with issue-scoped identity, allowing questions,
findings, and completion handoffs to reach the right thread.

### Local-first and recoverable

Repositories, worktrees, SQLite state, Codex processes, and the orchestration
daemon stay on your Mac. On restart, weft-codex resumes known Codex threads,
reattaches watchers, and restores pending bus delivery.

## What works today

- **Global CLI:** `weft-codex` launches the complete managed experience from any
  directory.
- **Native Weft Mode:** a third Codex mode exposes Workspace, Issues, Kanban, and
  Repositories while keeping chat in native Codex threads.
- **Multi-repository intake:** add several local repositories in one step and let
  repository profiling and relationship analysis run automatically.
- **Lead-owned decomposition:** users create Issues; the lead creates and
  dispatches tasks.
- **Worker isolation:** every task runs in its own repository worktree and Codex
  thread.
- **Durable coordination:** task state, thread identities, activity, and bus
  messages survive daemon restarts.
- **Native appearance:** Weft Mode consumes Codex semantic theme and locale
  context rather than maintaining a separate theme switcher.
- **Safe Mode:** `weft-codex --safe-mode` starts the official Codex experience
  without weftd or renderer injection.

The v0.1 line intentionally excludes multi-engine routing, human approval queues,
and migration from the original Weft app. CI/PR automation, Developer ID signing,
and Apple notarization are not yet included.

## Who it is for

weft-codex is for developers and technical leads who already use Codex and need
one product issue to move coherently across multiple repositories or parallel
implementation threads.

If one repository and one Codex thread already cover your workflow, the added
Workspace and Kanban structure may not be necessary. weft-codex does not replace
Git hosting, product management, or Codex itself.

## Install

The current Developer Preview supports macOS arm64 and expects the official app
at `/Applications/ChatGPT.app`.

Download the archive and checksum from
[GitHub Releases](https://github.com/SoloJiang/weft-codex/releases), then run:

```sh
shasum -a 256 -c weft-codex-0.1.1-macos-arm64.tar.gz.sha256
tar -xzf weft-codex-0.1.1-macos-arm64.tar.gz
cd weft-codex-0.1.1-macos-arm64
./install.sh

weft-codex doctor
weft-codex
```

The installer places a stable command in `~/.local/bin/weft-codex` and keeps
versioned runtimes under `~/.local/share/weft-codex/releases/`. Set
`WEFT_CODEX_PREFIX` to use another absolute installation prefix.

This preview is ad-hoc signed but not yet notarized. macOS Gatekeeper may require
confirmation for a browser-downloaded archive.

## Safety and data boundaries

- The Host launches the official Codex app; it does not copy, patch, overwrite,
  or re-sign it.
- CDP and weftd bind only to loopback.
- A dedicated Codex profile isolates Host-managed renderer state.
- If the current Codex CSP blocks the local Workspace surface, the Host enables
  compatibility mode only for its managed instance and restores it on exit.
- New data lives in `~/.weft-codex`. The original Weft app and `~/.weft` data are
  neither required nor migrated.

## Architecture today

```text
weft-codex CLI
├── Official Codex Desktop
│   ├── native Lead and Worker threads
│   └── injected Weft Mode surfaces
└── weftd
    ├── Workspace / Issue / Kanban API
    ├── Codex app-server orchestration
    ├── repository profiles and worktrees
    ├── durable Thread Bus
    └── SQLite state
```

The CLI is a headless lifecycle owner: it starts Codex and weftd, probes renderer
compatibility, mounts the Weft surfaces, and cleans up the processes and temporary
renderer state it created.

## Product skills

weft-codex ships Codex skills with the runtime (today: `weft-derive-test-cases`).
They are installed into `$CODEX_HOME/skills` automatically by:

- `./install.sh` / package upgrades
- `weft-codex doctor`
- normal `weft-codex` / `weft-codex attach` starts

Managed copies are marked with `.weft-managed` and refresh only when the
package skill frontmatter `version` changes. A local non-managed fork is left alone unless you force
it:

```sh
weft-codex install-skills
weft-codex install-skills --force
```

## Development

```sh
./scripts/start.sh              # build and launch from a source checkout
./scripts/install-cli.sh        # build and install the global CLI
./scripts/build-release.sh      # verify and assemble a release archive

cd ui && pnpm typecheck && pnpm build
cd launcher && pnpm typecheck && pnpm test
cargo test --workspace
git diff --check
```

## Project layout

```text
crates/app-server/   Codex app-server protocol and process runtime
crates/core/         orchestration, store, repository intake, worktrees, and bus
crates/daemon/       local HTTP, MCP, and UI daemon
launcher/            Codex Desktop lifecycle and renderer Host
ui/                  React Workspace, Issue, Kanban, and repository surfaces
packaging/           release installer and global CLI wrapper
scripts/             development, installation, and release commands
```
