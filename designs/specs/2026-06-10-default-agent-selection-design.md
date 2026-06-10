# 检测驱动的默认 agent 选择 — 设计

日期:2026-06-10
状态:已与操作者确认

## 背景与问题

Weft 支持 claude / codex / opencode 三个 coding CLI,但"默认用哪个"目前没有任何基于本地安装情况的选择逻辑,而是在三个互不相通的地方各自硬编码 `"claude"`:

1. 前端全局设置 `defaultTool`(`src/state/store.tsx`):`localStorage.getItem("weft-default-tool") ?? "claude"`,且该值只有 SettingsDialog 在读写,没有接到任何创建链路——是个空头设置。
2. Lead 会话(`src-tauri/src/lead_chat/commands.rs` 的 `lead_engine`):`tool: "claude"` 三处硬编码(engine、planner 注入、ask hook)。
3. 子任务(direction)的 tool:由 lead 调 `propose_directions` 时自由填,schema 里 `tool` 是裸 string,没有 enum、也不知道本机装了什么;lead 自己是 claude,惯性填 "claude",甚至可能填没安装的工具名。

检测能力已经存在但只用于展示:`detect.rs`(启动时合并 login shell PATH、`resolve_tool_path` 解析三个 CLI、codex 有 Codex.app bundle 兜底、最低版本检查)和 `tools.rs` 的 `detect_tools` 命令(installed / version / path / meets_min,只供 Settings 列表展示)。

预期行为:**默认 agent 基于本地实际安装的 CLI 选择,优先 codex;lead 和 worker 都用配置的工具;worker 在批准卡片上可二次选择。**

## 已确认的决策

| 决策点 | 结论 |
|---|---|
| Lead 的工具 | 跟随配置的默认工具(不再固定 claude) |
| 决策位置 | 后端决策(设置入 SQLite,后端检测 + 解析) |
| Issue 创建时的 lead 工具选择器 | 不加,一律用后端解析的默认 |
| Worker 的工具来源 | 配置默认,lead 不再分配;批准卡片上可二次选择 |
| 设置存储形态 | SQLite KV 表 `app_setting`(而非 JSON 配置文件) |
| 批量 confirm | 不逐条选工具,统一用默认;卡片是覆盖路径 |
| 旧 localStorage 配置 | 不迁移,直接弃用 |

## 设计

### 1. 后端:设置存储 + 默认解析

- 新增迁移:`app_setting(key TEXT PRIMARY KEY, value TEXT)` KV 表,SeaORM entity 同步。
- 新函数 `resolve_default_tool`(放 `detect.rs`),解析规则:
  1. 用户在 Settings 显式设置过(`app_setting` 里 `default_tool` 键存在)且该工具已安装 → 用它;
  2. 否则按 **codex > claude > opencode** 取第一个已安装的;
  3. 都没装 → 返回 `"codex"`(反正 spawn 不了,Settings/Onboarding 显示"未检测到 CLI"警告)。
- "已安装"用现有 `resolve_tool_path`(纯 PATH 查找 + Codex.app 兜底,不 spawn `--version`),每次调用直接解析,不做缓存。
- 新增命令:`get_default_tool`(返回 `{ tool, source: "user" | "detected" }`,前端展示用)、`set_default_tool`。`detect_tools` 保持不变,继续供 Settings 列表展示。

### 2. Lead:thread 盖章

- 迁移:`thread` 表加 `lead_tool TEXT` 列;存量行回填 `"claude"`(历史 thread 本来就是 claude 跑的,resume 的 native_id 只对 claude 有效)。
- `create_thread` 时后端自己调 `resolve_default_tool` 盖章到 `thread.lead_tool`,前端不传参。
- `lead_engine` 删掉三处硬编码 `"claude"`,改读 `thread.lead_tool`(engine 的 `tool` 字段、`inject_planner`、`inject_ask_hook`)。engine 的 per-tool 协议(`engine::per_turn`)与 `inject_planner` 的 codex 路径已存在,worker 在用,无新协议工作。

### 3. Worker:lead 不再分配工具,卡片二次选择

- `propose_directions` 的 MCP schema、`ProposedDirection`、`ResolvedDirection` 删掉 `tool` 字段;lead prompt 中 "(name, tool, the ONE repo each writes, reason, mandate)" 同步改为不含 tool。存量 plan JSON 里多出的 `tool` 字段 serde 自动忽略,无需数据迁移。
- Needs-you 卡(`WriteTriggerRow`,`src/board/NeedsYouView.tsx`)加紧凑工具选择器:
  - 选项只列已安装工具(来自 `detect_tools`);
  - 默认选中 `get_default_tool` 的结果;
  - 点批准时把选定 tool 传给 `approve_direction`(后端签名加 `tool` 参数,`planner::approve_direction` 用它创建 direction)。
- 批量 `confirm`(一键确认剩余提案):全部用 `resolve_default_tool` 的结果,不逐条选择。
- `ScopeReview` 的工具徽章改为显示"将使用的工具":已创建的 direction 显示其实际 `tool`,未批准的提案行显示当前默认。

### 4. 前端:Settings 迁移

- `store.tsx` 的 `defaultTool` 从 localStorage 改为后端命令读写(`get_default_tool` / `set_default_tool`)。
- 不迁移 localStorage 旧值 `weft-default-tool`,直接弃用;后端无显式设置时按检测优先序解析(正好落到 codex 优先的预期默认)。
- Settings 的默认工具选项只列已安装的;若用户设置的工具已被卸载,显示"已回退到 X"提示(`source: "detected"` 且与用户设置不一致时)。
- 顺带清理:`repo_ref.default_tool` 为无消费死字段,本次删除(迁移 + `add_repo_ref` 签名收窄)。

### 5. 错误处理与回退

- 已创建的 thread / direction 永远保留盖章时的 tool,不做静默切换。
- Spawn 时工具缺失(创建后卸载 CLI 的边角场景)按现状报错并清晰呈现;选择器只列已安装工具,正常路径不会发生。
- 用户显式设置的工具被卸载:解析自动回退到优先序中第一个已安装的,Settings 中可见提示;用户设置值保留不清除(重装后自动恢复生效)。

### 6. 测试

- `detect.rs`:`resolve_default_tool` 的优先序、用户覆盖、覆盖工具未安装回退、全缺省四种场景单测(注入假 PATH / 临时目录)。
- store:`app_setting` 表与 `thread.lead_tool` 列的迁移测试,存量行回填断言。
- planner:无 `tool` 字段的 proposal 解析;含旧 `tool` 字段的存量 JSON 兼容;`approve_direction` 带 `tool` 参数创建 direction。
- 前端:`npm run build` 通过;i18n 新增 key 中英双份。

## 不在本次范围

- 工具粒度的能力差异(如某工具不支持某 mandate)——一律视为等价可替换。
- per-repo 默认工具(`repo_ref.default_tool` 直接删除,不实现其原始构想)。
- Onboarding 流程改版(仅在未检测到任何 CLI 时复用现有警告面)。
