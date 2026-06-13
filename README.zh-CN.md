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

```text
需求 → 仓库地图 → 有边界的 agent 工作通道 → 仓库原生分支 → 实现 → PR / 合并 / 上线
```

**当前能力：** 本地多仓规划、仓库原生 worktree、原生 agent 会话、可 review 的 diff、
pre-PR checks、IM 问答、运行中防休眠、加密数据库备份。

**最终目标：** 你给一个需求，Weft 指挥你自己的 Claude Code、Codex、OpenCode 一路做到
PR、合并和上线。

## Weft 的不同之处

### 1. 编排跨仓库交付

你描述 feature、bugfix、refactor 或 spike。Lead agent 读取 workspace 的 repo map，
提出有边界的写入通道：哪个仓库需要写、为什么要写、由哪个 worker 执行。读取仓库是自由的；
只有写入会被确认、创建工作目录、追踪和 review。

### 2. 尊重你的原有习惯

Weft 和你已经信任的工具、仓库一起工作。

- **尊重用户工具：** Weft 驱动你自己的 Claude Code、Codex、OpenCode，保留它们的登录态、hooks、审批、sandbox、skills 和 session 身份。
- **尊重仓库习惯：** worktree 创建在目标仓库内：`<repo>/.worktrees/weft/<branch-name>`。分支名跟随该仓库已有风格，例如 `feat/*` vs `feature/*`、`fix/*` vs `bugfix/*`。
- **尊重团队经验：** 团队可以导入 Git 托管的 Skill 源，按全局或 workspace 选择性启用；个人或仓库自带的同名 Skill 仍可优先。会话开始前可以查看每个仓库最终生效的 Skills 和 Rules。

### 3. 本地运行，远程可达，数据可恢复

Agent 交付经常是长时间运行的桌面工作。Weft 可以在 session 运行时阻止系统空闲休眠；
开启 IM 桥时保持远程待命，让飞书指令随时到达；也可以把本地 SQLite 状态库加密备份到
私有 Git 远端，并单独导出 Recovery Key，方便换机或故障后恢复。

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

## 人不在电脑前，也能继续指挥

<p align="center">
  <img src="assets/diagrams/im-zh.svg" alt="IM 远程指挥：飞书卡片镜像权限请求和 agent 提问" width="940" />
</p>

Worker 产生的权限请求和 agent 提问可以镜像到飞书/Lark 交互卡片。移动端回复卡片，会解析到桌面端使用的同一个处理函数；不论在哪一侧回答，两边都会 patch 到同一个终态。

当前桥接覆盖：

- 权限请求与 agent 提问。
- Issue 到飞书话题的路由，让 Lead 消息双向流动；在飞书话题里发送
  `/bind <issue-id>` 即可绑定。
- 基于 `weft_global` MCP 工具的 Concierge 私聊入口。
- 每次恢复在线时，对待处理 Needs-you 做一次摘要同步。

绑定策略保持保守：首位私聊发送者可以成为 owner，群消息不能触发绑定，DB 错误 fail-closed。

## 产品界面

| Workspace 看板 | Issue 看板 |
|---|---|
| <img src="assets/screenshots/board-workspace.png" alt="Workspace 看板" /> | <img src="assets/screenshots/board-issue.png" alt="Issue 看板" /> |

| 仓库地图 | Lead 对话 |
|---|---|
| <img src="assets/screenshots/repo-graph.png" alt="仓库依赖图" /> | <img src="assets/screenshots/lead.png" alt="Lead 对话" /> |

## 架构

<p align="center">
  <img src="assets/diagrams/arch-zh.svg" alt="Weft 本地优先架构" width="940" />
</p>

Rust 后端负责本地 SQLite 状态库、git worktree 生命周期、headless agent 进程、Ask Bridge、本地 MCP bus、IM 桥、Skill 源、电源管理、加密备份和 sidecar 观测。React 前端负责 Workspace 看板、Issue 看板、Lead 对话、worker session、Observe/Diff、Settings 和 Needs-you 队列。

<p align="center">
  <img src="assets/diagrams/model-zh.svg" alt="Workspace、Issue、子任务、Session 模型" width="860" />
</p>

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
- 长任务可靠性：支持运行中防止空闲休眠、IM 桥远程待命，保证长时间会话和远程指令可达。
- 本地数据库备份：把加密后的 `weft.db` 快照 push 到私有 Git 远端，支持定时、退出时备份、恢复和 Recovery Key 导出。
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
