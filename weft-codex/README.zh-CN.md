<div align="center">

# weft-codex

### Codex 原生的多仓库交付工作区

weft-codex 把一个产品 Issue 转换成跨仓库、跨 Codex Thread 的协同交付。Lead
理解 Workspace 并创建仓库级 Task，Worker 在隔离 worktree 中执行；Kanban 和
持久化 Thread Bus 负责进度与交接，所有体验都发生在官方 Codex Desktop 中。

<sub>官方 Codex Desktop · React · Rust · SQLite · Codex app-server</sub>

[English](README.md)

</div>

## 30 秒了解

Codex 擅长在一个 Thread 中完成工作。weft-codex 补上的是跨 Thread、跨仓库协调
一个 Issue 的产品层。

```text
Workspace → Issue → Lead Thread → 仓库级 Task
                                  ↓
                         Worker Thread + worktree
                                  ↓
                         Thread Bus + Kanban
```

你创建 Workspace、加入属于同一产品的仓库，然后描述一个 Issue。Lead 读取仓库
关系，拆解 Task 并启动 Worker。你始终留在 Codex 中，weft-codex 在外围负责上下文、
worktree 隔离、状态、通信和重启恢复。

这里没有第二套聊天客户端，也没有让用户手工创建 Task 的表单。对话属于 Codex，
交付上下文属于 weft-codex。

## 产品体验

1. **创建 Workspace。** 一次加入一个或多个本地 Git 仓库。weft-codex 自动规范化
   仓库根目录、识别基线分支与 remote，并分析仓库画像和跨仓关系。
2. **描述 Issue。** Issue 是用户看到的问题或目标，也是 Workspace 看板上的交付单元。
3. **与 Lead 对话。** 原生 Codex Thread 获得 Issue 和仓库上下文，由 Lead 自己
   决定如何拆解并创建 Task。
4. **让 Worker 执行。** 每个 Task 获得独立 Codex Worker Thread 和仓库 worktree。
5. **保持 Agent 协同。** Lead 与 Worker 通过 Issue 范围内的持久化 Thread Bus
   通信，无需共享同一个 transcript。
6. **跟踪交付。** Kanban 展示排队、规划、执行、评审和完成状态，同时保留原生
   Codex Thread 供你直接对话。
7. **从中断恢复。** Thread 身份、Task 状态、worktree 和待投递消息都能跨 daemon
   重启恢复。

## 产品模型

| 产品对象 | 含义 |
|---|---|
| **Workspace** | 包含相关仓库和 Issue 的长期产品上下文。 |
| **Repository profile** | 自动采集的仓库身份、基线分支、结构和关系上下文，供 Lead 使用。 |
| **Issue** | 一个用户可见的问题或目标，拥有 Lead、Task、活动和整体进度。 |
| **Lead** | 理解 Issue、拆解工作、创建 Task 并协调 Worker 的原生 Codex Thread。 |
| **Task** | Lead 创建的一项仓库级工作；内部实现名称不会暴露在用户体验中。 |
| **Worker** | 在隔离 Git worktree 中执行一个 Task 的原生 Codex Thread。 |
| **Thread Bus** | Lead 与 Worker 之间持久化、Issue 范围内的通信通道。 |

## 为什么是 weft-codex

### 体感就是 Codex

用户看到的是官方 Codex Desktop。Lead 和 Worker 都是原生 Codex Thread；Weft
Mode 复用 Codex 的主题与语言上下文；打开 Task 会直接进入对应原生对话。Host
没有自己的窗口、Dock 图标或平行聊天界面。

### Issue 始终高于 Thread

Thread 是执行表面，不是产品模型。Workspace 上下文、Task 归属、仓库边界、进度
和交接始终归属于 Issue，即使某个 Thread 重启或被替换也不会丢失。

### 原生支持多仓库

一个 Workspace 可以包含多个仓库。Lead 获得仓库画像与关系后，创建最小而必要的
仓库级 Task；单仓库 Issue 使用同一套流程，不会增加额外负担。

### Lead 与 Worker 可以协作

Worker 不需要共享同一个上下文窗口。Thread Bus 为每个参与者提供持久化 inbox，
让问题、发现和完成交接准确到达目标 Thread。

### Local-first，并且可以恢复

仓库、worktree、SQLite 状态、Codex 进程和编排 daemon 都保留在本机。重启后，
weft-codex 会恢复已知 Codex Thread、重新挂载 watcher，并继续投递待处理消息。

## 当前可用能力

- **全局 CLI：** 从任意目录运行 `weft-codex` 即可启动完整体验。
- **原生 Weft Mode：** 作为 Codex 第三种模式提供 Workspace、Issue、Kanban 和
  Repository，同时把聊天保留在原生 Codex Thread。
- **多仓库录入：** 一次加入多个本地仓库，自动完成仓库画像与关系分析。
- **Lead 负责拆解：** 用户只创建 Issue，Task 由 Lead 创建和调度。
- **Worker 隔离：** 每个 Task 使用独立仓库 worktree 和 Codex Thread。
- **持久化协同：** Task 状态、Thread 身份、活动和 Bus 消息均可跨 daemon 重启恢复。
- **原生外观：** Weft Mode 消费 Codex 语义主题和语言上下文，不维护额外主题开关。
- **Safe Mode：** `weft-codex --safe-mode` 只启动官方 Codex，不启动 weftd，
  也不注入 renderer。

v0.1 明确不包含多引擎路由、人工授权队列和原 Weft 数据迁移。CI/PR 自动化、
Developer ID 签名与 Apple notarization 也尚未提供。

## 适合谁

weft-codex 面向已经使用 Codex，但需要让一个产品 Issue 跨多个仓库或并行实现
Thread 协同推进的开发者和技术负责人。

如果单仓库、单 Codex Thread 已经完整覆盖你的工作流，额外的 Workspace 和 Kanban
结构可能并非必要。weft-codex 不替代 Git 托管、项目管理或 Codex 本身。

## 安装

当前 Developer Preview 支持 macOS arm64，并要求官方应用位于
`/Applications/ChatGPT.app`。

从 [GitHub Releases](https://github.com/SoloJiang/weft-codex/releases) 下载压缩包
与校验文件，然后运行：

```sh
shasum -a 256 -c weft-codex-0.1.1-macos-arm64.tar.gz.sha256
tar -xzf weft-codex-0.1.1-macos-arm64.tar.gz
cd weft-codex-0.1.1-macos-arm64
./install.sh

weft-codex doctor
weft-codex
```

安装器会把稳定入口写入 `~/.local/bin/weft-codex`，并把版本化 runtime 保存在
`~/.local/share/weft-codex/releases/`。可以通过 `WEFT_CODEX_PREFIX` 指定其他
绝对安装前缀。

当前预览版使用 ad-hoc 签名，尚未 notarize。通过浏览器下载时，macOS Gatekeeper
可能要求用户确认。

## 安全与数据边界

- Host 启动官方 Codex，不复制、修补、覆盖或重签官方应用。
- CDP 与 weftd 只监听 loopback。
- Host 使用独立 Codex profile 隔离自己管理的 renderer 状态。
- 当前 Codex CSP 阻止本地 Workspace 表面时，Host 只为自己管理的实例启用兼容
  模式，并在退出时恢复。
- 新数据保存在 `~/.weft-codex`；不要求原 Weft 客户端，也不读取或迁移
  `~/.weft` 数据。

## 当前架构

```text
weft-codex CLI
├── 官方 Codex Desktop
│   ├── 原生 Lead / Worker Thread
│   └── 注入的 Weft Mode 表面
└── weftd
    ├── Workspace / Issue / Kanban API
    ├── Codex app-server 编排
    ├── 仓库画像与 worktree
    ├── 持久化 Thread Bus
    └── SQLite 状态
```

CLI 是无界面的生命周期管理器：负责启动 Codex 与 weftd、探测 renderer 兼容性、
挂载 Weft 表面，并在退出时清理自己创建的进程和临时 renderer 状态。

## 产品 Skill

weft-codex 会把产品 skill（当前是 `weft-derive-test-cases`）随 runtime 一起交付。
以下路径会自动同步到 `$CODEX_HOME/skills`：

- `./install.sh` / 版本升级
- `weft-codex doctor`
- 正常启动 `weft-codex` / `weft-codex attach`

托管副本带 `.weft-managed` 标记；只有 package skill 的 frontmatter `version` 变化时才刷新。若本地有未托管的修改，
默认保留，需要时可强制覆盖：

```sh
weft-codex install-skills
weft-codex install-skills --force
```

## 开发

```sh
./scripts/start.sh              # 从源码构建并启动
./scripts/install-cli.sh        # 构建并安装全局 CLI
./scripts/build-release.sh      # 验证并生成发行包

cd ui && pnpm typecheck && pnpm build
cd launcher && pnpm typecheck && pnpm test
cargo test --workspace
git diff --check
```

## 项目结构

```text
crates/app-server/   Codex app-server 协议与进程 runtime
crates/core/         编排、store、仓库录入、worktree 与 bus
crates/daemon/       本地 HTTP、MCP 与 UI daemon
launcher/            Codex Desktop 生命周期与 renderer Host
ui/                  React Workspace、Issue、Kanban 与仓库界面
packaging/           发行安装器与全局 CLI wrapper
scripts/             开发、安装与发行命令
```
