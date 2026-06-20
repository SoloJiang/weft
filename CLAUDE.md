# Repository Guidelines

## Project Structure & Module Organization

Weft is a Tauri v2 desktop app with a React frontend and Rust backend.

- `src/`: React + TypeScript UI. Key areas: `board/` for workspace/issue boards, `session/` for chat/observe/diff surfaces, `components/` for shared UI, `i18n/` for English/Chinese strings.
- `src-tauri/src/`: Rust backend. Key modules: `lead_chat/` for headless agent sessions, `store/` for SQLite/SeaORM entities and migrations, `bus/` for local MCP/thread bus, `git.rs` and `materialize.rs` for worktree handling.
- `src-tauri/tests/`: Rust integration tests.
- `assets/`, `public/`: screenshots, icons, and generated diagrams.

## Build, Test, and Development Commands

- `pnpm install`: install frontend dependencies.
- `pnpm dev`: run Vite for frontend-only iteration.
- `pnpm build`: run TypeScript checking and create the production frontend bundle.
- `pnpm tauri dev`: run the full desktop app in development mode.
- `pnpm tauri build`: build a release app bundle.
- `cd src-tauri && cargo test`: run Rust unit and integration tests.
- `git diff --check`: check patches for whitespace errors before committing.

## Coding Style & Naming Conventions

Use TypeScript for frontend code and Rust 2021 for backend code. Keep modules focused and follow the existing directory boundaries. Component files use `PascalCase.tsx`; helpers and state modules use lower camel or kebab style already present in the folder. User-facing strings must go through `src/i18n/en.ts` and `src/i18n/zh.ts`.

Rust production paths deny `unwrap`, `expect`, and `panic`; return `Result` and surface errors clearly. Avoid adding embedded terminal/TUI dependencies; Weft renders its own chat UI and uses terminal takeover only as an escape hatch.

Never nest ternary expressions — a `?:` inside another `?:`'s branch is banned in both TypeScript and Rust. A single, non-nested ternary is fine; for three or more branches use early returns, `if` / `else if`, a lookup map, or `match` (Rust). For multi-way JSX rendering, extract a small helper function or sub-component that returns the right element via `if`/`else` rather than chaining `cond1 ? a : cond2 ? b : c`.

## Testing Guidelines

Backend logic is covered with Rust unit tests next to modules and integration tests under `src-tauri/tests/`. Add tests for store migrations, worktree behavior, chat protocol parsing, planner scope, bus behavior, and verification logic when those areas change. Frontend changes should at minimum pass `npm run build`.

## Commit & Pull Request Guidelines

History uses short conventional prefixes such as `feat(plan): ...`, `fix(store): ...`, `polish(needs): ...`, and `chore: ...`. Keep commits scoped and descriptive.

Respect `.gitignore` strictly. Never `git add` ignored or out-of-scope paths, and prefer staging explicit files over `git add -A`/`git add .`. Review `git status` / `git diff --cached` before every commit and confirm each staged path belongs to this change. Internal planning and spec artifacts (e.g. anything under `docs/`, which is ignored) must never be committed; if you find such a path already tracked, untrack it with `git rm --cached` rather than leaving it in the repo.

PRs should include a concise summary, verification commands and results, linked issue/task when applicable, and screenshots or short recordings for visible UI changes.

When opening a PR, prefer the GitHub app/connector and fall back to `gh pr create`. Confirm the working tree is clean or contains only this PR's scope before pushing, and never bypass hooks with `--no-verify` (fix the real failure instead). After creating a PR, record and report: PR URL, PR number, base branch, head branch, head commit, and verification results — the head commit is the last-seen baseline for review monitoring.

## GitHub Remote Review Workflow

Opening a PR or pushing new commits to one triggers an automated review (the Codex review bot) on the GitHub remote. Pushing is not the end of the task: keep watching the PR until its review reaches a stable state — do not report "pushed" and stop.

Close the loop only when one of these holds:

- The Codex bot signals approval (e.g. a "Good"/LGTM reaction or an approving review).
- No unresolved, actionable review threads remain.
- The remaining threads are out-of-scope, speculative, or duplicate gates, and you have replied on the PR with an explicit push-back.
- The user tells you to stop.

Monitoring:

- Continuous watching must run as an independent monitor (event/notification subscription or timed polling). A one-off `gh pr view`, a manual refresh, or a blocking wait in the main task does not count as monitoring.
- Prefer subscribing to the PR's comments / reviews / reviewThreads / check-run events. If subscription is unavailable, poll on a timer — default every 5 minutes, or the cadence the user specifies.
- The monitor records PR URL, number, head branch, last-seen head commit, last-checked time, and the closure conditions. It only reads GitHub state and reports back to the working thread; it does not edit files, commit, push, resolve threads, or comment. On a head-commit change, a new review, an unresolved actionable thread, or an approval signal, it returns to the main thread to trigger handling.
- Read reviewThreads thread-aware (isResolved / isOutdated / inline path / line), not just flat comments. Each check reads reviewThreads / reviews / issue comments at minimum; with nothing new, report "no new actionable review" briefly — do not mistake that for final approval unless a closure condition is met.

Handling:

- A monitor/heartbeat report is standing authorization to continue handling the review: analyze each new comment, fix real problems with implementation + tests, push back explicitly on out-of-scope / speculative / duplicate threads, then commit, push, reply, and resolve the thread. Do not wait for the user to say "continue". Pause and ask only when a comment needs a product trade-off, exceeds the PR scope, or touches destructive operations, production access, or credentials.
- For every comment, judge real bug vs speculative first. Fix real issues with tests; for out-of-scope or duplicate gates, state why you are not changing it — the output of a review pass is "what you are not fixing and why", not turning every ⚠ into ✅. If the same spot is bounced repeatedly, the design is wrong — rewrite from first principles instead of patching.
- After handling, push the new commit, renew the monitor's last-seen head commit, and watch the next round.

Flag a thread as blocking when it touches behavior correctness, store migrations, chat-protocol/parsing, worktree/materialize behavior, planner scope, permission boundaries, data safety, secret leakage, or stability; mark style-only notes as non-blocking. Necessary tests must cover real user paths; user-facing strings stay in `src/i18n/en.ts` + `src/i18n/zh.ts`; Rust production paths return `Result`, never `unwrap`/`expect`/`panic`.

## Architecture & Configuration Notes

Do not write cross-repo wiring into canonical repositories. Use temporary launch flags, worktree-local ignored files, or Weft-managed state. Current delivery reaches reviewable worktree diffs with pre-PR checks; PR creation, CI/CD observation, and deployment orchestration are roadmap work.
