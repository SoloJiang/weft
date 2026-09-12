# Weft 迁移至 Codex Desktop 设计（2026-08-08）

> **产品形态与 Desktop 壳层已被
> [`2026-08-16-weft-third-mode-design.md`](2026-08-16-weft-third-mode-design.md)
> 取代。** 新实现以 08-16 为准。本文仍保留：编排 / bus / curator 决策、
> §5–6 线程绑定、§9 协议经验。历史 Stage 记录与当时的 iframe / Tier 1 假设
> 不再约束实现。
>
> 产品范围与用户可见决策以 [`PRODUCT.md`](../../PRODUCT.md) 为准。
> 协议字段以本机 `codex app-server generate-json-schema` 为准（见 §9）。

## 1. 背景与目标

停发 Weft Tauri 客户端，把产品能力移植进 Codex Desktop。聊天表面全部委托给
Codex 原生线程；Weft 退位为一个无头编排引擎（weftd）+ 一层嵌入 Codex Desktop
的薄 UI。

核心判断：Weft 最贵、最不差异化的部分是聊天 UI（ChatTimeline、transcript
解析、虚拟滚动、协议适配），这部分在与 OpenAI 的整个工程团队赛跑；真正差异化
的是编排层——多仓库、围绕 issue 的 lead/worker 模型、direction 并行探索、
bus 通信、worktree 生命周期、workspace/kanban。迁移保留后者，删除前者。

## 2. 范围

In：

- 多仓库（workspace / repo_ref / repo_profile 原样保留）
- 仓库录入与拆解：curator 分析管线（录入 → agent 分析 → repo_profile →
  repo map），执行层改造见第 5.5 节
- issue → lead + 多 direction 的编排模型
- lead/worker 均为真实 agent 会话，聊天在 Codex Desktop 原生渲染
- bus 通信（agent ↔ lead、跨 direction）
- worktree materialize 与 git 生命周期
- workspace home / kanban / issue detail / repo map 四个 UI 表面

Out（本阶段明确割舍）：

- 审批流 / Ask 桥（编排线程使用固定沙箱，异常 approval 由 weftd 拒绝并记录）
- 多引擎（claude / opencode；交互式线程全面转向 Codex）
- lead_chat 引擎全部（engine / proto / delta_hub / out_hub / window /
  revive / rewind / sentinels）——被 app-server 取代
- 整个 `src/session/` 聊天 UI、Needs You 治理（仅保留看板 attention 信号）

## 3. 总体架构

```
weftd（独立 daemon，Rust；剥离 Tauri 壳）
├─ store      workspace / repo_ref / thread(issue) / direction / worktree / bus
├─ orchestrator   lead/worker 线程创建、brief 渲染、事件 → 状态推导
├─ curator    仓库录入与拆解（分析 turn → repo_profile → repo map）
├─ app-server client   codex_app_server.rs Stage 2（全局多路复用，keyed by thread）
├─ bus server   MCP-over-HTTP（bus/server.rs 原样，身份从 URL path 派生）
├─ materialize / git   worktree 创建与分支生命周期
└─ http        REST/SSE（UI）+ MCP（agent 工具）

Codex Desktop（官方，不修改安装包）
├─ 原生聊天：lead 线程 + 每个 direction 线程（共享 ~/.codex store）
└─ 扩展 UI（weftd serve 的 web app，经 CDP 注入挂载）
   workspace home / kanban / issue detail → 点击跳转原生线程
```

关键性质：

- 所有业务逻辑在 weftd，对 Codex 升级免疫；扩展 UI 是纯展示层。
- 产品形态是 Desktop 第三种模式（见 `PRODUCT.md`）。浏览器页不是降级面；
  注入失败则 Weft 不可用，ChatGPT / Codex 仍可用。React UI 仍是同一份构建，
  `standalone` 只作开发预览。
- 每个 direction 与"一个 Codex 线程 + 一个 worktree"一一对应，与现有
  `direction` 实体（单 repo 写入、branch、mandate、五态状态机）天然吻合。

## 4. 数据模型变更

- `issue`：保留 Weft 的 `kind`（feature | bugfix | refactor | spike），并增加
  `lead_codex_thread_id`（string, nullable）；删除 `lead_tool` /
  `lead_command`（恒为 codex app-server）；`lead_meta` 废止（session 面板
  由 Desktop 取代）。
- `direction`：+ `codex_thread_id`（string, nullable）。status 推导来源由
  引擎事件改为 app-server 线程事件 + 明确的验收动作，五态不变
  （queued | planning | working | review | done）。
- `thread_binding`：每个原生 Codex thread 一行，记录 `issue_id`、可空的
  `direction_id`、`parent_thread_id`、`root_thread_id`、展示标题与
  `is_primary`。`issue.lead_codex_thread_id` / `direction.codex_thread_id`
  仍是 canonical Primary 指针；fork 只新增 binding，绝不替换 Primary。
  旧 canonical 指针在打开数据库时幂等回填到该表。
- `session` 实体废止（被 Codex 线程取代）；历史数据随客户端一起退役。
- 新流程保留 durable bus 记录；聊天历史由 Codex 原生线程持有，不再新增
  `lead_message` transcript store。
- 数据边界：weft-codex 使用独立 `~/.weft-codex` 与 fresh schema，不读取、迁移
  或修改原 Weft 数据。旧 Weft 数据迁移明确不在本项目范围。

## 5. 会话引擎替换：lead_chat → orchestrator

lead/worker 会话即 Codex 线程，由 weftd 经 app-server 创建与驱动：

- spawn lead：cwd = issue 主 repo（或 workspace 根），首条输入为 lead brief
  （issue 上下文 + workspace 仓库 id / base branch + 可用 MCP 工具说明）。Lead
  通过仅对 `lead` party 暴露的 `task_create` 拆出任务；工具要求完整 spec，校验
  repo 属于当前 issue workspace，并发出 `direction.updated`。worker 无法列出或
  调用该工具，用户界面也不提供手工新建任务。Issue 可以先于仓库创建；Lead 可
  在用户补录仓库后调用只读的 `repo_list` 刷新 repo id 与基线分支。
- spawn worker：materialize 出 worktree（repo_id + base_branch）→ 创建线程
  （cwd = worktree）→ 首条输入为 direction brief（按 mandate 渲染，
  plan+impl / impl-only，沿用现有 brief 模板）。`task_create` 持久化任务后立即
  进入 daemon 自动调度队列；重启时重新调度无 worker 线程的 queued 任务。
  用户无需逐个批准或启动，只有自动启动失败时才显示“重试启动”。
- 事件订阅：app-server notification → direction.status 推导
  （turn 进行中 = working；turn 完成 = review；error/idle 超时 = attention）。
- approval：编排线程固定使用 `approvalPolicy=never` 与角色对应沙箱（Lead
  read-only，Worker workspace-write）。正常工作在沙箱内直接执行；异常到达
  weftd 的 approval 请求只做协议级拒绝和记录，不做人工审批 UI。
- 线程对人类可见：线程写入共享 `~/.codex` store，出现在 Desktop 线程列表；
  人在 Desktop 打开线程即手动接管（此时 weftd 对该线程转为只观察）。

## 5.5 仓库录入与拆解（curator）

保留，且迁移顺手修复其已知脆弱点：

- 录入：repo_ref / repo_profile 实体与 Workspace home 的注册 UI 原样保留。
- 拆解执行层：从 `codex exec` one-shot（2026-06-18 spec 记录的 root cause——
  手写 argv 随 CLI 升级静默失效）切换为 app-server：
  - 每仓库分析 = 一个 **ephemeral** `thread/start`（不污染 Desktop 线程
    列表）+ 一个分析 turn；
  - 结构化产出走 `turn/start` 的 `outputSchema`（JSON Schema 约束最终
    消息），取代 `parse_repo_class` 式的文本解析；
  - 进度流 = app-server notifications → `repo_profile` run-state
    （idle | running | failed 语义沿用，重启可恢复）。
- 产物不变：tier / stack / summary / relations / monorepo components 仍落
  `repo_profile`；RepoMapView / RepoGraph（含 components 展开视图）作为
  扩展 web app 的第四个表面保留。

**已完成（2026-08-09，录入体验闭环）**：

- Workspace 仓库入口收敛为一个多仓导入动作；每行一个本地路径，单次最多
  64 个，逐项成功/失败，不因其中一个坏路径回滚已录入仓库。
- 路径由 daemon 规范化到真实 Git 根目录并校验至少有一个 commit；名称从目录
  推导。`base_ref` 不再由 UI 写死 `main`，按 origin/HEAD → main/master →
  当前非标准分支 → HEAD 的本地证据链识别，同时记录 origin URL 与默认分支
  可信标记。
- 工作区内按 canonical path 或 normalized origin URL 幂等去重；重新录入可修复
  旧行缺失的 remote/default metadata。
- 新仓库自动进入 profile 分析，全部新画像完成后自动刷新 relations/layers/
  repo map；UI 只保留单仓失败重试/重新分析，不再暴露割裂的“分析工作区”和
  “分析关系”主流程按钮。monorepo components 已可在仓库卡片展开。
- 浏览器降级面暂以多行绝对路径录入；Desktop adapter 落地时由 Host Bridge
  提供原生多目录选择，复用同一批量 API，不把文件系统权限交给 iframe。

**已验证（2026-08-08 晚，Stage 2.5 真机冒烟）**：

- `outputSchema` 在 ChatGPT 后端**非严格**：管线完整（turn/start →
  turn_context → prompt → Responses `text.format` json_schema strict），
  但模型仍可能输出散文 + fenced ```json。weft 的平衡括号扫描作为兜底
  解析保留（取最后一个含必需键的对象），brief 里显式要求"最终消息只
  有 JSON"后收敛。Ephemeral 线程确认不落 rollout 文件、不进 Desktop
  线程列表。
- agent 的 layers 自然产出是**分组形** `{label, rank, repos: [...]}`，
  而非 weft 时代的单库形 `{repo_id, label, rank}`——schema 改为分组形，
  解析两种都容。repo 引用按名字或数字 id 双通道解析。
- stack/components 需容错包装：agent 会把 array 字段产出成裸字符串
  （weft `lenient_confidence` 同类经验）。
- relations-only 重跑（`POST /api/workspaces/{id}/analyze-relations`）
  独立于全量分析，供画像未变时快速迭代。

## 6. Bus 通信机制

现有设计平移，仅投递侧更换：

1. agent 调用其 per-thread MCP endpoint（`/bus/:thread/:dir/mcp`，身份从
   URL path 派生，不可伪造）。
2. weftd bus server 记录并路由（BusRegistry 原样）。
3. 投递：orchestrator 经 app-server 把消息作为输入注入收件方 Codex 线程；
   线程在 Desktop 原生聊天中展示该消息。
4. 收件方通过自己的 MCP 工具回复，闭环。

创建线程时为每个线程配置其专属 bus MCP URL。**已验证（2026-08-08 spike）**：
`thread/start { config: { mcp_servers: {...} } }` 生效，per-thread 配置可用；
查询须用 `mcpServerStatus/list { threadId }`（不带 threadId 只列全局配置）。

进行中线程的消息注入语义，**已验证（2026-08-08 spike）**：

- 线程空闲：`turn/start` 正常开新 turn。
- 线程有活跃 turn：**禁止用 `turn/start`**——服务端返回成功（新 turn id、
  status=inProgress）但该 turn 实际从不执行（无 `turn/started`，活跃 turn
  结束后也不补跑），等于静默丢弃。
- 活跃中注入必须用 `turn/steer { threadId, expectedTurnId, input }`：
  实测重定向生效（steer 消息与模型回应均进入历史）。因此 weftd 必须按线程
  跟踪当前活跃 turn_id（与 `turn/interrupt` 的前置条件相同）。
- 读完整历史用 `thread/resume`（`thread/read` 默认只回元数据）。

**已验证（2026-08-08 晚，Stage 2 真机闭环，weftd + 真实 app-server）**：

- MCP 工具 annotations 是硬性要求：codex core 把无 annotations 的工具视为
  destructive/open-world（`requires_mcp_tool_approval`），在
  approvalPolicy=never + Managed 沙箱下审批请求被自动拒绝，工具返回
  "user rejected MCP tool call"。bus_post 须声明
  `{destructiveHint: false, openWorldHint: false}`，bus_read 加
  `readOnlyHint: true`。**注册成功 ≠ 可调用**，spike 只验证了前者。
- 全链路闭环：direction 线程按 spec 建文件并提交 → bus_post 成功 →
  orchestrator steer/start 注入 lead 活跃线程（消息以 user 身份入历史）→
  lead 自主核验 direction 分支。watcher 把 TurnEnd 折成 kanban 状态
  （working→review）。
- direction 需要 `spec` 字段（任务正文）：无烟情况下 agent 只能从 direction
  名字猜任务（把 hello.txt 建成了 hello）。已加列 + brief 渲染。
- 线程落库：rollout 文件即写，`state_5.sqlite.threads` 表即见（Desktop
  线程列表读这里）；`session_index.jsonl` 有滞后，不作为可见性依据。
  API 建线程 source 默认 "vscode"，名称留空（Desktop 显示回退名）。

**已验证（2026-08-09，human takeover 边界与溯源）**：

- 溯源标注：codex 0.145.0 **没有** `--session-source` CLI 旗标（该参数只
  存在于更新的 codex-rs git 源码），`threads.source` 只能是 "vscode"。
  可用杠杆是 `thread/start { threadSource }`（v2 协议的自由串字段），
  原样落库到 `threads.thread_source`（probe_thread_source.py）。weft-codex
  全部线程携带 `threadSource: "weft-codex"`，可与人手开的 vscode/Desktop
  线程区分。注意：thread/start 只建句柄，sqlite 行要首个 turn 才物化。
- turn 生命周期通知**不跨 app-server 进程**（probe_takeover.py）：第二个
  app-server 模拟 Desktop 接管（thread/resume + turn/start），weftd 的
  watcher 收不到对方的 `turn/started`/`turn/completed`。因此 foreign-turn
  检测只对同进程启动生效；真正的静默丢弃防线是**启动确认**：同进程
  `turn/started` 必然到达（probe_turn_started.py），inject 在每次
  `turn/start` 后等 watcher 路由到匹配 id（3s 超时），缺确认即判定丢弃、
  清除幻影 active_turn、消息留 inbox 并发 `bus.undelivered`。真机复核：
  接管期间发消息 → 状态停在 review（不再出现幻影 working）→
  bus.undelivered 到达。局限：外部 turn 结束无信号，积压消息要等下一次
  wake 才补投（agent 侧始终可 `bus_read` 拉取）。
- 同一 watcher 上的 `turn/started` 若 id 与己方 active_turn 不符仍标记
  foreign（同进程接管场景），其 TurnEnd 时清标并冲刷积压——该路径保留，
  若 codex 将来提供跨进程线程通知即自动激活。

**已验证（2026-08-09，Stage 4 真实多仓闭环与重启恢复）**：

- 隔离 workspace 含 `api`（base `trunk`）与 `web`（base `main`）；真实 Lead
  自主调用两次 `task_create`，daemon 自动 materialize 两个独立 worktree 并并行
  启动 Worker。两个 Worker 各提交 `MARKER.md`，经 `bus_post` 回报；Lead 收到
  两条注入消息后读取实际 commit 复核并输出跨仓汇总。真实分支与 commit 分别为
  `weft/cross-repo-marker/api-add-marker-file` / `e4107d6`、
  `weft/cross-repo-marker/web-add-marker-file` / `3abbb8e`。
- 状态实测：自动启动后 `working`，turn 完成后 `review`；“继续处理”创建新 turn
  并回到 `working → review`；两个“验收完成”最终均为 `done`。
- daemon 重启会先对持久线程执行 `thread/resume` 再重建 watcher；重启后向 Lead
  发送 `reattached-ok` 已进入同一原生线程并完成。Bus 增加 `delivered_at` 结算：
  未结算行在启动时恢复到 live inbox，listener ready 后重投；人为制造的 pending
  行在重启后自动送达并得到 `restored-pending-ok`，没有依赖下一条消息唤醒。
- Worker thread id 与首个 runtime status 在初始 turn 成功后原子发布；首 turn
  失败不会留下“已有 thread 但永远 queued”的不可重试状态。worktree 记录按
  direction 幂等，重试不会堆重复行。
- 异常 approval 防线按 method 回复：command/file-change 用 `{decision:
  "decline"}`，generic permission 用 `{permissions:{}}`；正常产品流仍无审批 UI。

## 7. UI 表面（weftd serve 的 web app）

- 实现基座：`ui/` 使用 React + TypeScript + Vite；shadcn/ui 作为组件源码层，
  只提供 Dialog、Button、Input、Select 等无业务状态的 primitive。
  shadcn 默认主题不是设计来源，所有颜色、字体、圆角、边框与交互状态继续映射
  Codex semantic token。前端构建产物由 weftd 作为静态文件提供。
- Workspace home：workspace 切换、repo 注册与状态。
- Kanban：direction 卡片按 status 分列；卡片默认只显示 repo / branch 与
  attention 信号，mandate 保留为内部编排字段。状态由编排事件推进，不支持通用
  拖拽、人工启动或任意改状态；用户在待审时执行“验收完成”，也可通过“继续处理”
  发送补充要求并让任务回到工作中。自动启动失败时提供唯一的“重试启动”恢复动作。
- Issue detail：lead 卡片 + direction 卡片列表，各自跳转 Desktop 原生线程；
  bus 活动时间线（**已完成 2026-08-09**：看板标题点击进入独立详情视图，
  时间线聊天式渲染 + `bus.message` SSE 实时刷新）。
- Repo map：仓库依赖图与 components 展开视图（RepoMapView / RepoGraph 平移）。
- 全部 i18n 字符串沿用 en/zh 双文件约束。
- **Surface 拆分（2026-08-09 已实现并实测）**：同一份 React 构建按 URL
  参数形成三种外壳，不复制业务组件：
  - `standalone`：开发预览面，保留 workspace selector、Kanban / 仓库入口
    topbar；weftd / SSE 健康状态不作为常驻产品元素，真实失败在具体操作处提示；
  - `sidebar`：Codex sidebar 内的全局导航，只放 workspace selector、Kanban、
    仓库、issue 列表和 attention 摘要；不放 Weft 品牌、语言/主题开关、聊天、
    长表单或 `direction` 术语。像 Weft 一样提供一级“新建 issue”动作，通过
    bridge 在主工作区打开“标题 + 类型”弹窗；创建后自动启动 Lead 并进入原生
    Codex 对话，不要求先添加仓库。Issue 主行点击同时展开会话树并打开其
    Primary Lead；右侧 chevron 只负责展开/收起，不改变当前线程。会话树按
    Lead / Tasks 分组，fork 归入其逻辑 Lead 或 Task 下；打开任何原生线程时，
    sidebar 自动切到对应 workspace、展开 issue 并精确高亮该线程；
  - `workspace`：Codex 主区域，只渲染 Kanban / 仓库 / issue detail，移除重复
    topbar 和重复的新建 issue 表单。Issue 详情不提供手工新建任务，任务由 lead
    chat 调用 `task_create` 产生并自动调度；lead / worker 沟通仍进入原生线程。
- sidebar 与 workspace URL 携带同一个随机 `bridge_id`，通过同源
  `BroadcastChannel` 做 ready/request 握手，同步 workspace、route 与白名单
  command；没有 `bridge_id` 时不开通道，避免两个独立浏览器窗口串状态。
  SSE 在两侧都只作失效提示，最终状态重新读取 weftd API。
- 组合实测覆盖：268px sidebar + 主工作区、Kanban → 仓库 → issue 路由联动、
  workspace surface 无重复 topbar、每个入口恰好一个 SVG、native select 恰好
  一个箭头（右侧 8px、垂直偏差 0）、横向 overflow 为 0，以及 Host Context
  驱动的中/英、明/暗主题实时切换。
- **主题同步（2026-08-09 修订）**：Codex 本身的主题可配置（dark/light、
  corner-radius scale），所以 web app 不硬编码主题，而是引用宿主 semantic
  token；但 CSS custom property **不会跨 iframe 自动继承**。同文档挂载时可
  直接读取，跨源 iframe 必须由第 7.6 节的 Host Context Bridge 读取允许列表、
  通过 `postMessage` 同步并写入 iframe 的 `documentElement.style`。独立浏览器
  使用 fallback，`prefers-color-scheme` 决定 dark/light。token 允许列表逐一
  核对自发行包（ChatGPT.app webview 资产）：
  表面 `--color-token-main-surface-primary` /
  `--color-token-dropdown-background` / `--vscode-sideBar-background`、
  文本 `--color-token-foreground` / `--color-token-text-secondary`、
  边框 `--color-token-border` / `--color-token-border-heavy`、
  accent `--color-token-primary`（经 link-foreground → blue-500
  `#0169cc`）/ `--vscode-button-hoverBackground` /
  `--color-token-button-foreground`、链接
  `--color-token-text-link-foreground`、状态色
  `--color-token-charts-green|yellow|red`、表单
  `--color-token-input-background|border`、字体 `--font-sans` /
  `--font-mono`、圆角 `--radius-lg|md|sm`（随宿主 corner-radius-scale
  缩放）。已实测三层：dark fallback（`#131313`/`#fcfcfc`/`#0169cc`）、
  light fallback（`#fcfcfc`/`#0d0d0d`）、React bridge receiver 的版本、locale、
  token 白名单与圆角映射已通过模拟 host envelope 实测。**2026-08-09 已完成
  真实 Desktop 端到端验证**：宿主 `lang=en → zh-CN` 与
  `--color-token-primary → #ff00aa` 的临时变更实时到达 sidebar/workspace 两个
  OOPIF，随后恢复原值；两侧语言与 token 均同步回宿主状态。

## 7.5 模式切换（Weft mode）

产品形态以 `PRODUCT.md`「Product Form」为准：Weft 是与 ChatGPT / Codex 同级的
第三种模式，不是插件或浮层。本节只写 Desktop 怎么把这个形态挂上宿主开关。

入口级模式开关，挂载到应用既有的顶层模式切换上：应用本身已有
ChatGPT（everyday work / `isEverydayWorkMode`）与 Codex 双模式，Weft 作为
**第三个并列模式**加入，而不是自造一套独立开关：

- Weft 模式下隐藏 Codex 常规会话列表与无关聊天入口，只保留 workspace /
  issue / kanban / repo map 表面；线程聊天仍使用原生线程视图（唯一不隐藏
  的原生表面）。
- 侧边栏头部的两个原生 action（搜索 = Command menu、铃铛 = activity view）
  同属"无关聊天入口"，但**不是删掉了事，而是同位换语义**：隐藏原生两个，
  在同一槽位放 Weft 自己的搜索与收件箱，按钮克隆自原生控件以继承宿主样式。
  模式该改变的是能力的**范围**，不是它**存不存在**——头部在模式之间保持
  同一形状，切模式时不会突然空一块。
  - 按钮只是触发器：点击 `postMessage` 给 sidebar iframe（`search.open` /
    `inbox.open`），面板一律由 React 渲染；角标数由 sidebar 上报
    （`inbox.count`），agent 只负责画那个数字，**从不读原生角标状态**。
    这条边界就是 §7.6 的"renderer 只做 thin surface agent"。
  - 搜索范围 = 当前 workspace 的 issue / direction / repo / artifact / 会话，
    对 `/api/issues` 已返回的 board 做客户端过滤，不新增接口。
  - 收件箱 = `direction.attention` + `bus.undelivered`。**`bus.parked` 不进
    收件箱**：它表示人正在该线程上说话、turn 结束会自动 flush，不需要任何人
    做任何事；把它列出来只会让收件箱塞满"读着读着就自己好了"的条目。
  - 槽位锚点是 `optional` 而非 `subtractive`：它缺失只应让两个入口改在 Weft
    sidebar 自己的头部渲染（Host Context 的 `headerActions: "fallback"`），
    **能力不丢，只降级位置**。开发预览没有宿主头部，也走这一支。
  - **不要给这两个入口绑快捷键。** 实测宿主已占用 `⌘K` / `⇧⌘P`（Command
    menu）与 `⌥⌘U`（activity view），且绑在 Electron 菜单 accelerator 层，
    renderer 大概率收不到、也就拦不掉。Weft 搜索改用 `/` 聚焦。详见
    `docs/compat/codex-builds.md` §5.10。
- 线程归属数据化：weftd 创建线程时设置 `threadSource` 并遵循命名规范，
  模式过滤基于 weftd store 中的 issue/direction 归属关系判定，不靠 DOM
  结构猜测。
- 实现分层：
  - Tier 1（默认，additive）：原生 UI 不动，仅注入 workspace 入口与视图。
  - Tier 2（Weft mode，subtractive）：隐藏原生侧边栏会话区并挂载
    workspace 导航。按版本做结构 probe，不匹配时 **fail-open 回 Tier 1**
    （宁可露出原生 UI，不可藏错或藏坏）。
- 开关形态（优先级从高到低）：
  1. 既有模式切换器加第三项 Work / Codex / **Weft**——用户已理解的模式
     语义，一等 UI 锚点也比侧边栏内部结构更稳定；
  2. launcher flag 决定启动默认模式；
  3. 自有 toggle（持久化在 weftd 侧）——模式切换器结构 probe 失败时的
     回退，保证 Weft mode 永远可达。
- 产品不提供浏览器降级。Tier 2 是唯一产品形态；模式开关或减法探针失败则
  fail-closed，不能进入 Weft，不能退回“浏览器里的 Weft”或 Codex 模式里的
  附加行。`standalone` 只作开发预览。

桌面嵌入：launcher 以 `--remote-debugging-port` 启动 ChatGPT.app（不改包、
不破签名），经 CDP 挂载上述 web app 到侧边栏入口；Host 在 launcher 进程，
renderer 仅 thin surface agent，通信走 CDP binding。未知版本 fail-closed
进入 safe mode（不注入，Weft 不可用，官方 Codex 仍可开）。

**已完成（2026-08-09，真实发行版）**：Launcher 已能独立启动/回收官方 Codex
与 weftd，也能 attach 专用实例；两级 readiness gate 分开等待 CDP target 与 React
hydration。Renderer Agent 通过 document-start 注入、MutationObserver 重挂载，
同一页面只保留一个 sidebar root、workspace root 与 style。原生模式菜单实测为
ChatGPT / Codex，Weft 作为第三个 `menuitem` 注入；进入 Weft 前先经原生菜单切到
Codex 底座，退出 ChatGPT/Codex 后宿主 label/ARIA 正确恢复。Weft item 只显示一个
选中图标，英文副标题不截断。

Codex 路由过渡会同时保留一个不可见、`inert` 的旧 `main` 与一个可见主区域；
workspace adapter 不使用 `querySelector("main")`，而是只选择在 viewport 内、
可见、可交互且不位于 `inert` / `aria-hidden` 祖先下的最大语义 `main`。路由的
`inert` / `aria-hidden` / `hidden` 状态变化会触发重新挂载；capability probe
使用同一套判定，避免“探针通过但 UI 被挂进隐藏页面”的假阳性。

build 6119 与当前复验的 build 6321 均由 `frame-src` 阻止 loopback iframe；
`Page.setBypassCSP` 对已
提交文档不追溯生效，必须先启用再 reload。Host 已按“首次握手失败 → 专用实例启用
bypass → 安装 document-start script → reload → 双 frame handshake”执行；兼容
细节只进入 launcher 日志 / `doctor`，不作为常驻产品 UI。退出时禁用 bypass 并
reload 恢复 CSP。真实 Lead deep-link
不再依赖 renderer 内无效的 `location.assign(codex://...)`，而是按
`data-app-action-sidebar-thread-id` 命中原生行：实测从 Kanban 打开同一 Lead Thread，
主区由 Codex 原生渲染，再点 sidebar Kanban 返回 workspace。刚创建的线程可能晚于
Weft UI 数秒出现在原生列表，因此 Host 在返回失败前按固定、有限退避等待语义行；
UI 等待 action result，期间显示“正在打开”，最终失败则显示可重试错误，不再
静默吞掉点击。OS `codex://` 只保留 standalone anchor 的降级语义，不作为专用
profile Host 的 fallback，避免 deep link 被另一个 Codex 实例接走。原生多目录 picker
通过受控 Host action + macOS folder dialog 接入，iframe 不获得文件系统权限。

官方 plugin 仍可作为 skills / MCP / 分发层，但当前公开 manifest 没有任意
sidebar contribution point，因此 sidebar 与模式外壳属于 renderer adapter，
不能假设安装一个普通 plugin 就能完成。

## 7.6 Host Context Bridge 与注入生命周期

Desktop 接入不允许 UI 自己猜宿主状态。launcher / renderer agent 与 React UI
之间定义版本化消息协议：

```ts
interface HostContextV1 {
  version: 1;
  theme: "light" | "dark";
  locale: string;
  tokens: Record<AllowedCodexToken, string>;
  mode: "work" | "codex" | "weft";
  view?: "workspace" | "thread";
  projectId?: string;
  threadId?: string;
  sidebarCollapsed: boolean;
}
```

- iframe URL 必须携带 launcher 写入的 `host_origin`；UI 只向该 origin 发送
  `{ source: "weft-codex-ui", type: "weft:host-context-request", version: 1 }`。
  host 回包固定为 `{ source: "weft-codex-host", type:
  "weft:host-context", payload: HostContextV1 }`，UI 同时校验 `event.source ===
  window.parent`、`event.origin` 与 payload schema。没有 `host_origin` 时只允许
  同源 parent，独立顶层页面不发送请求。
- `tokens` 只允许固定白名单；renderer 用 `getComputedStyle` 读取解析值，React
  UI 写入自己的根节点。宿主主题或圆角变化后重新发布快照。
- `locale` 由宿主明确传递，iframe 不使用自己的 `navigator.language` 猜测宿主
  语言；独立浏览器才回退到系统语言。
- `view` 明确区分 Weft workspace overlay 与 Codex 原生 thread surface。
  `threadId` 只有在 `view === "thread"` 时参与 sidebar 选中态；返回 Kanban / 仓库
  后即使原生 DOM 仍暂时保留 active thread，也不能残留错误高亮。
- 所有 frame → host 请求使用固定 origin、版本、action allowlist 与 payload
  schema；禁止 `postMessage("*")` 执行高权限动作。
- 注入使用独立 profile、loopback-only CDP、document-start 脚本与 ready
  handshake；renderer 被导航或替换后重新探测、重新挂载，不能重复注册入口。
- 若当前发行版 CSP 阻止本地 iframe，只允许 launcher 对专用实例启用 CSP
  bypass；该诊断状态只在 CLI / `doctor` 中可见，不占用 sidebar。CDP 端口无
  应用层认证，launcher 存活期间仅允许可信本机代码，退出时关闭端口与子进程。
- capability probes 至少覆盖：主 renderer、页面 mount、侧边栏入口、原生
  thread route、模式切换器、主题/locale、titlebar drag region。Tier 2 任一
  subtractive probe 失败即回 Tier 1；基础挂载失败则 safe mode。
- 对当前 native thread 的反向定位先查 `thread_binding`；若是刚由 Codex fork
  出来的未知 thread，则用元数据级 `thread/read` 读取明确的 `forkedFromId`，
  有界向上追溯到已知祖先后写回整条 binding 链。无已知祖先的普通 Codex thread
  保持 unbound，不猜测归属。UI 通过 `POST /api/threads/resolve` 完成该解析。
- 原生线程跳转优先使用稳定 route/deep link；composer prefill 只用于创建普通
  人类线程，不替代 weftd 通过 app-server 创建的 lead/worker 线程。
- 该定制不会改变 Codex Computer Use 的自操作安全边界。当前发行版在尝试读取
  `com.openai.codex` 时明确拒绝；launcher / CDP 是独立、用户启动的本机集成
  通道，不得包装成让 Codex 绕过自身 computer-use 限制的后门。

## 7.7 同类 taskboard 的吸收边界

对照独立 taskboard 产品后，吸收下面这些产品原则：

- 看板是 daemon 状态的清晰投影，不在浏览器里再造 agent runtime；写操作走
  明确 API，SSE 只作提示并重新读取真值。
- workspace → repositories → issue → tasks 是用户主路径；卡片状态、归属仓库、
  执行模式与 attention 都可扫读，但用户界面不暴露 `direction` 内部术语。
- 同一份 React 构建服务 Desktop 第三种模式；`standalone` 只作开发预览，
  不是产品降级面。不能维护一套“浏览器 Weft”和一套 Desktop Weft。
- 交互 primitive 统一到 shadcn/ui 源码层，但不继承它的品牌视觉；宿主主题、
  locale、原生线程和模式入口仍由 Codex 决定。

明确不吸收的范围：

- 不重建 AI Chat、模型选择器、sandbox picker 或 transcript timeline；聊天继续
  使用 Codex 原生线程。
- MVP 不加入 Dashboard、甘特图、通用 Workflow Builder、周期任务等横向项目
  管理能力；它们会稀释多仓库 issue 编排主线。
- 评论、附件、issue 关系、乐观并发版本是可选的后续基础设施，但不得替代
  lead/worker、bus、worktree 与 curator 数据模型。

## 7.8 独立发布与回退

发布物不依赖源码 checkout：

- Host 由 Bun 编译为单个 macOS 原生可执行文件，用户不需要安装 Node；
- 同包携带 release `weftd` 与构建后的 React assets；Host 根据自身可执行文件
  位置解析 archive layout；
- 不发布第二个 `.app`。Host 是无窗口、无 Dock 图标的后台进程，只负责启动
  `/Applications/ChatGPT.app` 中的官方 Codex 并接入；发行包保留 `doctor`、
  `probe`、`attach` 等诊断入口；
- 用户入口统一为无窗口命令 `weft-codex`：无参数完成 weftd、官方 Codex 启动和
  注入并默认进入 Weft mode；`weft-codex --safe-mode` 不启动 weftd、不注入 UI，
  只打开专用 profile 的官方 Codex；退出 Host 会恢复 CSP 并回收自己创建的进程；
- 本地构建使用 ad-hoc 签名。对外分发必须在相同产物上增加 Developer ID 签名、
  notarization 与发布渠道校验，不把重签官方 Codex 纳入任何步骤。

## 8. 删除清单

这是 **weft-codex 新项目的排除清单**，表示这些能力不得进入新 runtime；不要求、
也不授权修改或清理原 `weft` 仓库：

- `src/session/` 全部（ChatTimeline / WorkerConversation / LeadTab /
  SessionInfoPanel / transcriptBits / MindMapEditor 等）
- `src-tauri/src/lead_chat/` 全部
- `src-tauri/src/ask.rs`、approval 相关命令
- claude / opencode 适配（`claude.rs` / `opencode.rs` 及 engine routing）
- `curator.rs` 的 `run_agent_once` exec 路径（分析改走 app-server，见 5.5）
- Tauri 壳（commands.rs 的 UI 命令、窗口、WebView 配置）；`codex.rs` 的
  exec 路径在 Stage 2 切换后删除

## 9. Spike 验证项（先于一切实现）

Desktop 相关（1、3）仍需运行时闭环；app-server 相关（2、4、5）**已于
2026-08-08 验证**，
脚本与日志在 `docs/superpowers/spike-app-server/`：

1. `open -a ChatGPT --args --remote-debugging-port=...` + CDP attach 稳定，
   注入脚本活过页面导航与会话切换；探针需同时覆盖 Tier 2 所需的侧边栏
   结构与顶层模式切换器锚点（见 7.5）。**PASS（2026-08-09）**：已读取
   当前安装 `ChatGPT.app`（bundle id `com.openai.codex`；初验版本 `26.727.51351` /
   build `6119`，2026-08-10 复验版本 `26.803.41515` / build `6321`），发行 bundle
   存在 `data-app-action-sidebar-scroll|section|
   section-heading|project-row|thread-row` 等语义属性；`launcher/` 已实现安装检测、
   CDP target 选择、renderer hydration probe 与 `safe-mode | additive | weft-mode`
   分类测试；专用 profile 真机完成 Weft mode 菜单注入、document-start reload、
   React DOM 替换后的单实例重挂载、双 OOPIF handshake、CSP bypass 可见/退出恢复
   及独立进程回收。
2. weftd 经 app-server 创建的线程出现在 Desktop 线程列表，事件订阅正常。
   **PASS**：`thread/start` 后 `session_index.jsonl` 同步写入、
   `state_*.sqlite threads` 表异步落库（秒~分钟级滞后，`thread/list` 读该表），
   `thread/resume` 正常；真实 Lead/Worker 线程已在 Desktop 原生列表及主区打开，
   daemon 重启后的 watcher 与 bus delivery 也已恢复。
3. 从扩展内以编程方式跳转指定线程。**PASS**：`codex` scheme
   已在 ChatGPT.app Info.plist 注册；app bundle 内确认路由
   `codex://threads/<threadId>` 被官方 Chrome 扩展的 "Open in app" 用于
   线程跳转（另有 `codex://threads/new`、`codex://settings/connections`）。renderer
   内直接 `location.assign(codex://...)` 实测无效，因此 adapter 改用当前发行版的
   语义 thread-id 行。真实 workspace 的 Lead 链接已原地打开
   `019fe64c-d5a0-7cb2-b6e6-295d62e34dab`，原生行变为 active，主区显示完整 Codex
   对话；sidebar Kanban 可切回 workspace。
4. per-thread MCP 配置能力。**PASS**：见第 6 节。回退方案不触发。
5. 进行中线程注入语义。**PASS**：见第 6 节。关键结论：bus 投递在活跃 turn
   期间必须用 `turn/steer`，`turn/start` 会被静默丢弃。

附带的协议经验（写入 weftd 实现注意）：

- app-server stdout 单行可超 64KB，reader 必须用大缓冲。
- `approvalPolicy: "never"` + 只读沙箱下无任何 approval/elicitation 请求。
- API 创建的线程 `source` 默认为 `"vscode"`；weftd 应设置
  `threadSource` / `sessionStartSource` 做正确分类。
- weftd 自行持久化 thread_id 与活跃 turn_id，不依赖 `thread/list` 的
  索引时效。

任何一条不成立：不能进入 Weft 模式（产品壳不可用），bus/编排仍在 weftd
内成立，但不向用户提供浏览器替代面。

## 10. 分阶段落地

- Stage 0：Spike 五项（days 级）。
- Stage 1：weftd 抽离——`src-tauri` 核心剥为独立 daemon（axum server 已有），
  Tauri 客户端同期继续可用。
- Stage 2：orchestrator——`codex_app_server.rs` Stage 2 接线 + lead/worker
  spawn + bus 投递 + 事件 → 状态推导。本迁移最大的一块。
- Stage 3：扩展 UI v1——workspace home + kanban + issue detail（**2026-08-09
  已迁移至 React + TypeScript + shadcn/ui primitive 并完成浏览器实测**：三视图、
  standalone/sidebar/workspace 三 surface、双 surface 路由与 workspace 同步、状态
  投影、SSE 自刷新、亮/暗主题、host context 模拟、桌面/手机断点与原生线程深链）。
- Stage 3.5：Desktop adapter（**已完成 2026-08-09**）：安装检测、语义 probe、
  三档兼容分类、专用 profile Launcher、document-start 重挂载、CSP bypass 显示与
  恢复、原生第三模式、双 Surface、Host Context、原生 Thread 切换均已真机闭环。
- Stage 4：独立交付（**已完成 2026-08-09**）——用户入口收敛为无窗口
  `weft-codex` 命令，不发布第二个 App；单命令已从最终 archive 真机启动官方
  Codex、weftd 并完成 `tier=weft-mode` 双 surface 注入。`--safe-mode` 实测只启动
  官方 Codex、不启动 weftd；退出后 CDP/weftd 端口与专用 profile 进程均为零。
  Release pipeline 通过 55 个 Codex app-server、46 个 core、25 个 Launcher 测试，
  UI typecheck/build、daemon HTTP + MCP roundtrip、签名与 SHA-256 均通过。runtime
  已移除其他引擎 parser/routing 与可接受 approval 的 API；旧数据及原 Weft 仓库
  明确不在范围内。

## 11. 风险与对冲

- Codex 平台演进吃掉编排层：编排逻辑在 weftd 内、UI 是薄皮，官方若推出
  原生多 agent 编排，迁移成本限于 UI 层。
- 注入脆弱性：fail-closed。挂不上第三种模式则 Weft 不可用，ChatGPT / Codex
  仍可用；不对用户提供浏览器降级。
- iframe 隔离：主题 token、locale、当前 project/thread 不会天然跨 frame；
  必须通过版本化 Host Context Bridge 同步，不允许 UI 靠 DOM 猜测。
- CDP 暴露：仅绑定 loopback 与专用 profile，launcher 退出即回收；CSP bypass
  只对专用实例开启并在 UI 中可见。
- app-server 协议漂移：`generate-ts` / `generate-json-schema` 生成绑定，
  min-version 硬开关（沿用 Stage 2 既定策略）。
- 多引擎损失：接受。若未来需要，非 Codex 引擎以无头任务形态回归，
  不进入 Desktop 聊天表面。
