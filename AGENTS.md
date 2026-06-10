# Repository Guidelines

## Project Structure & Module Organization

Weft is a Tauri v2 desktop app with a React frontend and Rust backend.

- `src/`: React + TypeScript UI. Key areas: `board/` for workspace/issue boards, `session/` for chat/observe/diff surfaces, `components/` for shared UI, `i18n/` for English/Chinese strings.
- `src-tauri/src/`: Rust backend. Key modules: `lead_chat/` for headless agent sessions, `store/` for SQLite/SeaORM entities and migrations, `bus/` for local MCP/thread bus, `git.rs` and `materialize.rs` for worktree handling.
- `src-tauri/tests/`: Rust integration tests.
- `assets/`, `public/`, `designs/`: screenshots, icons, generated diagrams, and design references.

## Build, Test, and Development Commands

- `npm install`: install frontend dependencies.
- `npm run dev`: run Vite for frontend-only iteration.
- `npm run build`: run TypeScript checking and create the production frontend bundle.
- `npm run tauri dev`: run the full desktop app in development mode.
- `npm run tauri build`: build a release app bundle.
- `cd src-tauri && cargo test`: run Rust unit and integration tests.
- `git diff --check`: check patches for whitespace errors before committing.

## Coding Style & Naming Conventions

Use TypeScript for frontend code and Rust 2021 for backend code. Keep modules focused and follow the existing directory boundaries. Component files use `PascalCase.tsx`; helpers and state modules use lower camel or kebab style already present in the folder. User-facing strings must go through `src/i18n/en.ts` and `src/i18n/zh.ts`.

Rust production paths deny `unwrap`, `expect`, and `panic`; return `Result` and surface errors clearly. Avoid adding embedded terminal/TUI dependencies; Weft renders its own chat UI and uses terminal takeover only as an escape hatch.

## Testing Guidelines

Backend logic is covered with Rust unit tests next to modules and integration tests under `src-tauri/tests/`. Add tests for store migrations, worktree behavior, chat protocol parsing, planner scope, bus behavior, and verification logic when those areas change. Frontend changes should at minimum pass `npm run build`.

## Commit & Pull Request Guidelines

History uses short conventional prefixes such as `feat(plan): ...`, `fix(store): ...`, `polish(needs): ...`, and `chore: ...`. Keep commits scoped and descriptive.

PRs should include a concise summary, verification commands and results, linked issue/task when applicable, and screenshots or short recordings for visible UI changes.

## Architecture & Configuration Notes

Do not write cross-repo wiring into canonical repositories. Use temporary launch flags, worktree-local ignored files, or Weft-managed state. Current delivery reaches reviewable worktree diffs with pre-PR checks; PR creation, CI/CD observation, and deployment orchestration are roadmap work.
