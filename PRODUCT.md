# Product

## Register

product

## Users

Developers who deliver features and fixes that span several repositories and
want coding agents (Claude Code, Codex, OpenCode) to carry the work from intent
to reviewable multi-repo diffs and, next, pull requests, not just chat. They think in tasks ("add a discount code to
checkout", "fix #4821"), not in worktrees or sessions. Their context: a focused
desktop session beside an IDE, several work lines in flight at once, wanting to
*supervise* delivery, not babysit terminals. They step in when something is
genuinely stuck, not to push every step forward.

## Product Purpose

weft is a local-first, no-server **delivery hub** where coding agents drive
multi-repo work from a **Task toward shipped code**. The north star is
**automation**: you state a task; weft plans it, decides which repos to touch,
spawns the agents, coordinates them, verifies the result, and drives it out the
door. You supervise and handle exceptions — you are not a required checkpoint in
the loop.

Delivery is **phased**: current code reaches reviewable local worktree diffs
with pre-PR checks; the next product boundary is clean pull requests. The north
star is to carry it the rest of the way — **merge, then deploy across environments
(staging → production)** — so the unit of *done* is shipped code, not an open PR.

The shape of the work:

1. **Understand** — a workspace is a logical list of repo references. A
   workspace-level **curator** agent profiles each repo (one-line role,
   interfaces, stack) and weft builds a **cross-repo dependency graph**. This
   map is the fuel for the core trick below.
2. **Decompose (the wow)** — you give a **Task** (PRD / bug / refactor / spike /
   link; PRD is just one kind). A per-thread **lead** agent classifies it, then
   uses the repo map to derive **scope** (which repos each direction will
   *write*) and split it into **directions** — automatically. Reads are
   unmanaged: an agent may read any repo freely, so only the write set is
   scoped, materialized, and confirmed. No other tool turns one task into
   "which repos, and who does what" across a fleet.
3. **Deliver, automatically** — each write-repo gets an isolated git worktree;
   the lead spawns a **worker** per direction (heterogeneous tools allowed),
   hands each a structured **brief** (scope + interface-contract + acceptance),
   and drives them to convergence over a per-thread **bus**. Workers' output is
   gated by **executable verification** (lint / type / test / contract /
   configured in-worker review skill), not by a human nod. Green currently means
   a reviewable worktree diff with pre-PR evidence; the next boundary is opening
   or updating a PR, and the roadmap flows on through **merge and
   environment-aware deploy (staging → production)**; red retries within bounds,
   then escalates.

**Home is a conversation, not a terminal grid.** The primary surface is the
**lead** (your main chat + control tower): read-only across the repos, it plans,
derives scope, and drives workers — it does not write code. Sessions carry
`role = curator | lead | worker`. Workers report **structured summaries + diff
stat** through the bus; the lead never ingests their raw transcripts. Each
session's surface is a weft-rendered conversation driven over the native CLI's
structured stream; the native TUI stays one takeover away in your own terminal.

**The human handles exceptions, not the assembly line.** weft adds **no approval
gate of its own**. The only blocking interruptions are the tools' own permission
prompts (passed through verbatim, never overridden) plus a configurable
irreversible-action boundary (e.g. merging a protected branch, or a production
deploy). Everything else runs; "what's waiting on me" is the rare exception,
surfaced at the top of every view.

**Delivery is phased — now reviewable diff, next Task → PR, the goal Task →
shipped.** Current code reaches local worktree diffs with light pre-PR
verification. The next boundary is a PR per repo: weft drives the native CLIs
(it doesn't bypass hooks), so opening a PR naturally triggers the repo's own
checks. The north star reaches past the PR — weft **drives merge, then deploy
across environments (staging → production)** — by *orchestrating the repo's
existing pipelines*, never re-implementing CI/CD. The unit of *done* becomes
shipped code; irreversible steps stay gated by the configurable boundary above.

It is explicitly **not** a terminal emulator, and not a "watch the agents go"
dashboard. It is the workspace-and-automation fabric the agents deliver inside.

## Brand Personality

Composed, exact, quietly alive. Three words: **calm, precise, native-fast.**
The voice is an expert peer's, not a vendor's: it states what is happening and
gets out of the way. No hype, no hand-holding, no decoration for its own sake.
When something is running, the interface should feel like a well-instrumented
control room — legible, everything in its place, motion only when something
actually changed. Dark by default but fully at home in light too (both are
designed, system-following + toggleable).

## Anti-references

- **Generic SaaS pastel gradients** — purple-blue gradient heroes, rounded
  card seas, emoji decoration. The template-site sweetness.
- **Heavy decoration / glassmorphism** — blur stacks, glow, big drop shadows,
  flourish animations. Showy, attention-scattering, slow.
- **Dry enterprise back-office** — gray-on-gray, dense tables with zero
  rhythm, zero craft. Usable but joyless.
- **Terminal/"matrix" aesthetic** — weft frames terminals; it is not one.
  Avoid neon-green-on-black, scanlines, faux-CRT, monospace-everything.

## Design Principles

1. **Automation is the north star.** The default path is autonomous: task in,
   shipped code out. Every surface is built for *supervising* that flow, not for
   driving it step by step. If a screen assumes the human pushes each step forward, it
   fights the product.
2. **The human handles exceptions, not the line.** weft adds no gate of its own.
   Surface the rare blocker (a tool's permission prompt, a true agent
   escalation, a hard conflict) loudly; let routine flow pass silently. Never
   manufacture a checkpoint where automation would do.
3. **Structure is the product.** The `workspace → thread → direction → scope`
   model, and the repo map that powers scope decomposition, are what weft sells.
   Surfaces make that structure legible and editable; the terminal is a leaf.
4. **Cross-repo scope decomposition is the wow.** One Task becoming "these
   repos, this split, in this order" is the irreplaceable moment. Protect its
   legibility: show what was inferred, let the human correct it, learn from the
   correction.
5. **Trust through verification, shown.** Because no human gates the work, the
   board is a *trust dashboard*: each card carries its acceptance signals (tests
   x/y, contract match, review skill result where configured) with expandable provenance. Green
   you trust; red or escalated you open. Be honest where a repo isn't verifiable.
6. **The board flows itself; the human acts, not drags.** A two-level kanban
   (workspace = threads, thread = directions) is a live projection of agent +
   git state. Cards move on their own through the lifecycle; the human's verbs
   are Approve / Answer / Open / Review / Merge. "Needs you" aggregates real
   exceptions at every level and is always the most prominent thing.
7. **Drive native, render product.** Sessions drive the native CLIs headless
   through their structured streams, rendered as weft's own conversation;
   native state (permissions, sessions, config) is mirrored, never overridden.
   Surface and observation are decoupled — a session can run in weft, in its
   own app, or taken over in your terminal, observed the same way throughout.
8. **Hide the mechanism, present the decisions.** worktrees / headless agent
   processes / MCP bus / add-dir / sidecar are plumbing — they recede into Inspect. What the user owns
   stays first-class: the task, scope, branch / PR / diff, tool choice, brief,
   effective skills. Every abstraction ships with a real escape hatch (true
   path / open terminal) and a readable failure.
9. **Calm under parallelism.** Many threads and directions at once must read as
   composed, not busy. Density without noise; motion only when something
   actually changed; the eye always finds the one thing that needs it.
10. **Bilingual from day one.** zh / en, two layers — UI strings AND agent output
    language. Never hardcode user-facing strings; internal state enums stay
    English, code/identifiers always English.

## Accessibility & Inclusion

- WCAG AA contrast on the dark surface: body text ≥ 4.5:1, large/secondary
  ≥ 3:1. No light-gray "elegance" text that fails to read.
- **Status is never color-only.** Every run state pairs color with a shape,
  icon, and/or label (running / waiting / approval / error / paused), so it
  survives color-blindness and grayscale.
- Full keyboard navigation; visible focus rings on every interactive element.
- `prefers-reduced-motion` honored everywhere — reveals and transitions degrade
  to instant or a simple crossfade.
