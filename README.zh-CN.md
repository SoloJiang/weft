<div align="center">
  <img src="public/weft-logo.svg" alt="Weft" width="220" />

### 本地多仓交付编排器，指挥你自己的 Coding Agents

Weft 是一个本地多仓交付编排器。你给它一个需求，它会指挥你自己的 Claude Code、
Codex、OpenCode 跨多个仓库推进，把需求从意图一路带向实现、合并和上线。

<sub>Tauri v2 · React 19 · Rust · SQLite · Native Coding-Agent CLIs</sub>

[English](README.md)
</div>

<p align="center">
  <img src="assets/readme/weft-overview.png" alt="Weft 概览：仓库进入 Lead 工作区，多个 scoped worker 产出检查后的 review diff" width="940" />
</p>

## 30 秒看懂

Weft 不是终端网格，也不是云端 agent runner。它是在你的需求、原生 coding agents、
仓库、分支、检查和发布流程之间做协调的本地编排层。

最终目标链路：

```text
需求 → 仓库地图 → 有边界的 agent 工作通道 → 仓库原生分支 → 实现 → PR / 合并 / 上线
```

### 1. 跨仓库范围拆解

你描述 feature、bugfix、refactor 或 spike。Lead agent 根据 workspace 的 repo map
判断哪些仓库需要写、每条写入通道为什么存在、应该由哪个 worker 执行。读取仓库是自由的；
只有写入会被声明、确认、创建工作目录并持续追踪。

### 2. 尊重用户自己的工具习惯

Weft 驱动你已经在用的原生工具：Claude Code、Codex、OpenCode。它不替代这些工具的
登录态、hooks、审批、sandbox、skills 或 session 身份。权限请求会镜像到 Weft，
但不会被绕过；需要原生体验时，也可以一键回到自己的终端接管。

### 3. 尊重仓库自己的协作习惯

Weft 不把内部命名规则强加给你的仓库。新的工作目录创建在目标仓库内：

```text
<repo>/.worktrees/weft/<branch-name>
```

分支名会根据该仓库已有风格推断：`feat/*` 还是 `feature/*`，`fix/*` 还是 `bugfix/*`；
只有冲突时才追加数字后缀。Weft 的路由和状态留在本地数据库里，你的 git 历史继续保持
它原本的样子。

### 4. 导入团队经验，保留个人习惯

团队可以导入 Git 托管的 Skill 源，同步到本地后，再按「所有 Workspace」或「当前
Workspace」选择性启用。你自己熟悉的原生 CLI Skills 仍然保留；如果仓库里有同名
Skill，仓库自己的版本优先。Weft 也会展示每个仓库最终生效的 Skills 和 Rules，让你
在会话开始前知道「哪些能力会被带进去、来自哪一层」。未来工作区规则包（workspace
rule packs）也应沿用同一套模型：团队给默认值，用户按需启用，仓库自己的规则最后说了算。

## 实际工作流

<p align="center">
  <img src="assets/diagrams/flow-zh.svg" alt="从任务到范围确认再到可验证 worktree diff" width="940" />
</p>

1. 在 Workspace 中添加已有仓库。
2. 新建 Issue，并和 Lead agent 讨论目标。
3. 查看 Lead 提出的写入通道：仓库、原因、工具和执行授权。
4. 确认哪些通道可以创建 worktree。
5. Worker 以 headless 原生 CLI 会话运行，并流式进入 Weft。
6. 你只处理真正的阻塞、查看 diff，并在 PR 前运行检查。

人处理异常，不推动流水线。

## 产品模型

- **Workspace**：一组逻辑仓库，以及仓库画像、规则和工具配置。
- **Issue**：一条面向用户的工作线，可以是 feature、bugfix、refactor 或 spike。
- **子任务（Sub-task）**：一个具体 worker 通道，目前绑定一个写入仓库。
- **Session**：一个原生 agent 会话，绑定到某个 worktree。

内部存储仍用 `thread` 表示 Issue 层，用 `direction` 表示子任务层。面向用户的文档和 UI 统一称为 **Issue** 与 **子任务**。

## 产品界面

| Workspace 看板 | Issue 看板 |
|---|---|
| <img src="assets/screenshots/board-workspace.png" alt="Workspace 看板" /> | <img src="assets/screenshots/board-issue.png" alt="Issue 看板" /> |

| Lead 对话 | 仓库地图 |
|---|---|
| <img src="assets/screenshots/lead.png" alt="Lead 对话" /> | <img src="assets/screenshots/repo-graph.png" alt="仓库依赖图" /> |

## 架构

<p align="center">
  <img src="assets/diagrams/arch-zh.svg" alt="Weft 本地优先架构" width="940" />
</p>

Rust 后端负责本地 SQLite 状态库、git worktree 生命周期、headless agent 进程、Ask Bridge、本地 MCP bus、IM 桥、skill source 和 sidecar 观测。React 前端负责 Workspace 看板、Issue 看板、Lead 对话、worker session、Observe/Diff、Settings 和 Needs-you 队列。

<p align="center">
  <img src="assets/diagrams/model-zh.svg" alt="Workspace、Issue、子任务、Session 模型" width="860" />
</p>

## IM 远程指挥

<p align="center">
  <img src="assets/diagrams/im-zh.svg" alt="IM 远程指挥：飞书卡片镜像权限请求和 agent 提问" width="940" />
</p>

worker 产生的权限请求和 agent 提问可以镜像到飞书/Lark 交互卡片。移动端回复卡片，会解析到桌面端使用的同一个处理函数；不论在哪一侧回答，两边都会 patch 到同一个终态。

当前桥接覆盖：

- 权限请求与 agent 提问。
- Issue 到飞书话题的路由，让 Lead 消息双向流动；在飞书话题里发送
  `/bind <issue-id>` 即可绑定。
- 基于 `weft_global` MCP 工具的 Concierge 私聊入口。
- 每次恢复在线时，对待处理 Needs-you 做一次摘要同步。

绑定策略保持保守：首位私聊发送者可以成为 owner，群消息不能触发绑定，DB 错误 fail-closed。

## 当前能力

- Workspace 仓库 add/clone/create，以及确定性 Repo Profile。
- 基于 repo map 的 scope 提案：Lead 会说明每条通道写哪个仓库、为什么写。
- 仓库原生 worktree 和遵循目标仓库风格的分支名。
- Claude Lead 会话，带 planner MCP 和写入范围确认。
- Lead action card：在对话里添加、克隆或创建仓库。
- Claude Code、Codex、OpenCode worker 会话。
- Weft 自有 chat timeline，支持排队、打断、resume、slash commands 和附件。
- Ask Bridge 统一展示工具权限请求，支持 Allow、Always、Full、Deny。
- Skill 源管理：支持导入团队公共 Skill 仓库，保留个人已熟悉的本地 Skills，并按全局或 workspace 选择性启用。
- 有效配置预览：展示每个仓库实际生效的 Skills 和 Rules，以及来源层级和覆盖关系。
- sidecar 观测 Claude jsonl、Codex rollout jsonl 和 OpenCode SQLite。
- 从物化 worktree 展示 diff 和 pre-PR checks。
- Workspace、Issue、子任务重命名和级联删除。
- 中英双语 UI。

尚未产品化：自动创建 PR、受保护分支合并编排、CI/CD 观测、部署编排、工作区规则包（workspace rule packs）、团队 marketplace 同步、长期语义 Curator。

## 本地开发

```bash
npm install
npm run dev          # Vite 前端
npm run build        # TypeScript 检查 + 生产前端 bundle
npm run tauri dev    # 完整桌面应用
npm run tauri build  # release app bundle
cd src-tauri && cargo test
git diff --check
```

## 目录结构

```text
src/
  board/                Workspace 和 Issue 看板
  session/              chat、observe、diff、权限请求
    blocks/             chat timeline 的富块
    useRepoActions.ts   Lead action card 触发的添加/克隆/新建仓库
  components/           共享 React UI
  i18n/                 英文和中文文案
src-tauri/src/
  lead_chat/            headless agent 会话引擎
    sentinels.rs        解析 <weft:action_card> / <weft:list_repos/> 控制符
    repo_state.rs       注入到 Lead prompt 的 <repo_state> 快照
  im/                   IM 桥（Channel trait + 飞书适配器，ws + cards）
  store/                SQLite/SeaORM entities 与 migrations
  bus/                  本地 MCP/thread bus + human-ask notifier
  ask.rs                权限 Ask 注册中心（桌面 + IM 同源）
  git.rs                仓库和 worktree 操作
  materialize.rs
assets/
  screenshots/          README 截图
  diagrams/             架构图和模型图
  readme/               README 概览生成图
```

## 设计约束

Weft 通过结构化的 headless 接口驱动原生 CLI，并渲染自己的产品 UI。正常 chat surface 不引入嵌入式终端/TUI 依赖；终端接管仍作为需要原生 CLI 时的逃生入口保留。
