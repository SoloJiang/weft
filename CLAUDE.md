# Repository Guidelines

## What you're working on

Weft is the home monorepo. The Tauri desktop app lives in `src/` + `src-tauri/`. Shared Rust crates live in `crates/`. The Codex Desktop / weftd line lives in `weft-codex/` (ui, launcher, crates, scripts, specs). `~/.weft` and `~/.weft-codex` stay separate. DESIGN.md and chat shells stay per product.

## Layout gotchas

- `src/board/`, `src/session/`, `src/components/`, `src/i18n/` — Weft UI surfaces.
- `src-tauri/src/lead_chat/`, `store/`, `bus/`, `git.rs`, `materialize.rs` — Weft backend adapters.
- `crates/weft-scheduler`, `weft-bus`, `weft-brief`, `weft-worktree`, `weft-slug` — shared Rust (no UI/TS/CSS).
- `weft-codex/` — Codex Host, weftd, and tracked specs (`weft-codex/docs/` is committed).
- `src-tauri/tests/` — Rust integration tests for the Tauri app.
- Root `docs/` is gitignored planning material; never commit it. If it is tracked by mistake, untrack with `git rm --cached`.

## Hard constraints

- User-facing strings go only through `src/i18n/en.ts` and `src/i18n/zh.ts`.
- Rust production paths return `Result`; no `unwrap` / `expect` / `panic`.
- No nested ternaries in TypeScript or Rust. Prefer early returns, `if` / `else if`, lookup maps, or `match`.
- Multi-way UI/state: derive ONE discriminated value, then map it exhaustively (`Record`, `switch`, or a small status view). See `src/components/ui/StatusChip.tsx`. Do not re-derive the same booleans at every call site.
- Command/handler registry edits are high-risk; diff neighboring entries so nothing adjacent is dropped.
- Recursive filesystem work needs tests for symlink containment, large-directory truncation, and skipped artifact directories.
- UI path tokens may be relative or carry line/column suffixes; absolute filesystem openers are a separate path.
- Do not write Weft wiring into *user* canonical repositories. Use launch flags, worktree-local ignored files, or Weft-managed state. Shared crates in this monorepo are the in-tree exception (weft is the home repo).
- Prefer isolated worktrees for feature work so unrelated dirty state stays out of the main checkout.
- Avoid adding embedded terminal/TUI dependencies; Weft owns the chat UI and uses terminal takeover only as an escape hatch.

## Verify before you claim done

- Frontend/TS: `pnpm build`
- Shared + weft-codex Rust: `cargo test --workspace`
- Tauri Rust: `cargo test --manifest-path src-tauri/Cargo.toml` (scoped is fine when the change is local)
- Patch hygiene: `git diff --check`
- Visible UI: reproduce on the running Tauri/WebView surface when behavior matters

Add or extend tests when changing store migrations, worktree/materialize behavior, chat protocol parsing, planner scope, bus behavior, or verification logic.

## Git / PR baseline

- Conventional commits: `feat|fix|polish|chore(scope): ...`
- Stage explicit paths only. Never scoop unrelated dirty files with `git add -A` / `git add .`.
- Ready-for-review PRs by default. Include a concise summary, verification commands/results, linked issue when applicable, and UI evidence for visible changes.
- Prefer the GitHub app/connector; fall back to `gh pr create`. Never `--no-verify`.
- After opening a PR, record URL, number, base, head branch, head commit, and verification results. That head commit is the monitoring baseline.

## PR closure bar

Opening or pushing a PR is not done. Keep watching until the review is stable.

A PR is truly mergeable only when **all three** hold:

1. CI is green on every platform check.
2. Codex has all-cleared the PR itself — a 👍 on the PR body, or an approving review.
3. `mergeable == MERGEABLE` (no base conflict). If the base advances into conflict, merge the latest base (prefer a merge commit over force-push), then re-check.

Also required: unresolved review threads are handled. Fix real bugs with tests; explicitly push back on speculative, out-of-scope, or duplicate notes. Zero open threads alone is not enough.

Monitoring must be continuous (event subscription or timed polling, default about every 5 minutes), tracking PR URL, number, head branch, last-seen head commit, and last-checked time. Prefer GraphQL `reviewThreads` (`isResolved` / `isOutdated` / path / line) over flat comments. On a new review, head-commit change, or approval signal: fix or reply, push, resolve, and keep watching until the three-way bar holds.

Treat threads as blocking when they touch behavior correctness, store migrations, chat protocol/parsing, worktree/materialize, planner scope, permission boundaries, data safety, secret leakage, or stability. Style-only notes are non-blocking. Pause for product trade-offs, scope breaks, destructive ops, production access, or credentials; otherwise a monitor signal is standing authorization to continue.
