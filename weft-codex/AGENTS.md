# Repository Guidelines

## What you're working on

weft-codex: Weft 的编排能力移植到 Codex Desktop 的独立项目。Rust workspace
（`crates/`）+ 未来的 TS launcher 与 React UI。产品形态与 Desktop 壳层的规范在
`docs/specs/2026-08-16-weft-third-mode-design.md`（canonical）。
协议 spike 与 bus 投递经验仍在
`docs/specs/2026-08-08-codex-desktop-migration-design.md` §5–6、§9。

## Hard constraints

- Rust production paths return `Result`; no `unwrap` / `expect` / `panic`
  （crate 级 clippy deny 已配置，测试豁免）。
- No nested ternaries. Prefer early returns, `if` / `else if`, lookup maps,
  or `match`.
- Multi-way state: derive ONE discriminated value, then map it exhaustively.
- UI 用户可见字符串只走 i18n 双文件（en/zh），UI 落地时执行。
- 改控件外观前先读 `DESIGN.md`：宿主是真相且数值都是真机实测；视觉属性写
  `index.css`（unlayered 永远赢 Tailwind utilities，堆在组件上的视觉类不生效）。
- app-server 协议变更以 `codex app-server generate-json-schema`（本机安装
  版本）为准；协议经验记录在 08-08 spec §9。
- Bus 投递语义：活跃 turn 用 `turn/steer`，空闲用 `turn/start`；
  **活跃期间 `turn/start` 会被静默丢弃**（spike 实证）。

## Verify before you claim done

- `cargo test --workspace`
- `git diff --check`
- Daemon 冒烟：隔离 `WEFT_CODEX_HOME` 启动 weftd，curl `/healthz` 与一个
  MCP roundtrip。
- 动了 TS：`pnpm --dir launcher test`；动了 UI 再加 `pnpm --dir ui typecheck`
  / `test` / `build`。
- 改了外观必须真机看一眼：隔离 profile 起 Codex 注入，**先灌出能触发该 UI 的
  数据**，再 CDP 截图核对。单测与 build 证明不了外观；空 workspace 的截图同样
  证明不了 issue 行的改动。

## Git baseline

- Conventional commits: `feat|fix|polish|chore(scope): ...`，提交信息用中文。
- Stage explicit paths only. Never `git add -A` / `git add .`.
- docs/ 在本仓库是 tracked 设计文档（与 weft 仓库的 gitignored 习惯不同）。
