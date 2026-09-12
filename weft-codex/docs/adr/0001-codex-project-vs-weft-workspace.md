# ADR 0001：Codex Local Project 与 Weft Workspace 的关系

- 状态：**Accepted**（2026-08-10）
- 相关：[#3](https://github.com/SoloJiang/weft-codex/issues/3)（N0-02）、umbrella [#1](https://github.com/SoloJiang/weft-codex/issues/1)
- 影响：N0 的 sidebar 形态、R4 的 Project knowledge 归属

## 背景

weft-codex 有 `workspace`（一组 repo）这个概念，Codex Desktop 也有 "Local Project"
（一组本地文件夹）。两者形状接近，用户语言也接近，存在形成"双重概念"的风险——这正是
umbrella #1 风险表里列出的一条。本 ADR 决定二者的关系。

## 决策

**不做 1:1 映射。`workspace` 保持为 repo membership 的唯一权威；Codex Local Project
不进入 weftd 的数据模型，至多在创建 Workspace 时作为一次性的目录候选来源。**

对外用户语言继续是 **Workspace**，不改名为 Project。

## 依据

前三条是决定性的，且都有实测支撑。

### 1. weftd 创建的线程对 Codex 的 project 层完全不可见

在本机（Codex 26.803.41515 / build 6321）读取 `~/.codex/.codex-global-state.json`
与 `~/.codex/state_5.sqlite`：

| 统计 | 值 |
|---|---|
| Local Projects | 4 |
| `thread-project-assignments` 条目 | 3 |
| `projectless-thread-ids` 条目 | 32 |
| `thread_source = "weft-codex"` 的线程 | 5 |
| ↳ 其中有 project 归属的 | **0** |
| ↳ 其中被列为 projectless 的 | **0** |
| ↳ 两个名单都不在的 | **5** |

weft 线程不是"没有 Project"，而是 Desktop 的 project 记账**根本不知道它们存在**。
把 Workspace 绑到 Project 上，等于把我们的核心实体绑到一个不认识我们对象的层。

### 2. weftd 说的协议里没有 Project

`thread/start` 的参数是 `cwd`，不是 `projectId`（`ThreadStartParams` 见
`docs/compat/codex-builds.md` §4 的结构核对）。`state_5.sqlite` 的 `threads` 表也只有
`cwd` 与 git 快照列，**没有任何 project 列**。Project 是 Electron/UI 层叠加在核心之上的
本地状态，weftd 经 app-server 根本没有参与它的途径。

### 3. Project 是我们不拥有、也无法安全写入的私有本地状态

`local-projects`、`thread-project-assignments` 等键都存在单个 JSON 文件
`~/.codex/.codex-global-state.json` 里，由 Electron 主进程读写并只向同实例的其它窗口广播。
它没有版本号、没有稳定契约、也没有 probe 机制——比 DOM 锚点更脆弱，因为 DOM 至少还有
capability probe 和三档降级兜底。写入它则意味着与 Codex 进程竞争同一个文件。

### 4. rootPaths 是文件夹，repo_ref 是仓库

Local Project 的结构是 `{ id, name, rootPaths: string[], createdAt, updatedAt }`。
`rootPaths` 是裸路径，未必是 git 仓库；而 `repo_ref` 携带 `base_ref`、`remote_url`、
`base_ref_is_default`，并挂着 `repo_profile` 与 `repo_relation`。二者信息量不对等，
"映射"实际上是我们单方面补齐 Codex 不持有的信息。

### 5. 两套 worktree 互不相干

Codex 自己的 worktree 在 `~/.codex/worktrees/<uuid>/<repo>/`；weft 的 worktree 由
`crates/core/src/worktree.rs` 管理，实测 weft 线程的 cwd 形如
`/private/tmp/weft-codex-*/worktrees/<task>/<repo>`，位于任何 Project 的 `rootPaths`
之外。即使想靠 cwd 反推 Project 也推不出来。

## 逐条回答 #3 的问题

**是否 1:1？** 否。二者是平行概念，不建立实体关联。

**Codex Project 的多目录如何映射到 repo membership？** 不做持续映射。允许的用法只有
一次性预填：新建 Workspace 时可把当前 Project 的 `rootPaths` 作为候选目录列出，用户确认
后由 Weft 走既有的 repo 录入流程（`repo_intake`）建立 `repo_ref`。导入后所有权归 Weft，
Project 后续增删目录不回流。

**primary folder 与 repo membership 的关系？** 无关系。primary folder 决定 Codex 新会话
的默认 cwd 以及 `AGENTS.md` / skills / `config.toml` 的发现位置；Weft 的 worktree cwd 由
Task 所属 repo 决定，不读 primary。仅在上述"预填"场景可用 primary 决定候选顺序。

**N=0（用户没有任何 Project）时 Workspace 如何表达？** 照常表达。Workspace 不依赖
Project 存在——这已经是当前行为，本 ADR 只是确认它是有意为之而非疏漏。

**Thread / worktree 的 cwd 如何归属到 Project？** 不归属。Weft 侧的归属由 `thread_binding`
表达（thread → issue/direction），与 Codex 的 `thread-project-assignments` 平行且互不干扰。

**是否给 `workspace` 表加 project 关联字段？** **现在不加。** 加了就要处理 Project 被删除、
改名、换 `rootPaths`、跨设备不同步，以及我们无法写入该文件因而只能单向读。翻案条件明确：
**当 Codex 在 app-server 协议层暴露官方的 project API 时重新评估**——那时它才是我们能正常
参与的契约，而不是一个私有 JSON。

**用户语言：对外叫什么、是否改名、迁移代价？** 继续叫 **Workspace**，不改名。改叫 Project
会与原生 Project 正面冲突：用户会以为是同一个东西，而二者语义不同（一个是文件夹集合，
一个是带 base branch / remote / 画像的交付仓库集合）。改名要动 UI 文案、i18n 双文件、
API 路径与全部文档，收益为负。

**宿主是否需要上报 `projectId`？** 不上报，并且**建议移除** `HostContextV1` 中的
`projectId` 占位字段。它在 `ui/src/host-context.ts` 被声明并校验，但 `launcher/src/renderer-agent.ts`
从不填充、`ui/src` 也从无消费者——留着会让后来者误以为这条链路存在。移除属于独立的清理项，
不在本 ADR 的范围内执行。2026-08-16 同文档壳落地时 `host-context.ts` 已删除，见
[ADR 0002](./0002-same-document-shell.md)。

## 备选方案与放弃理由

**A. 严格 1:1，直接读取 `~/.codex/.codex-global-state.json`。** 放弃：私有格式、无版本、
无探针、无法写入；且依据 1 表明 weft 线程根本不在该层，读到了也对不上。

**B. 双向同步（Weft 改动回写 Project）。** 放弃：需要写入我们不拥有的文件，与 Codex 主进程
竞争，损坏用户状态的代价远高于收益。

**C. Workspace 可选弱引用一个 `project_id`（只读、可为空）。** 暂缓而非否决：在没有官方 API
之前，它带来的维护面（失效引用、改名、跨设备）超过导航上的收益。"是否加字段"一问里的翻案
条件同样适用于它。

## 后果

- **对 #5 / sidebar 形态**：Weft section 不需要挂在某个 Project 之下，作为 sidebar 的独立
  区块即可。这与 #5 已落地的形态一致（Tier 2 接管 sidebar、Tier 1 追加入口行）。
- **对 R4**：Project knowledge、repo/service map 与长期 Agent 一律挂在 Weft `workspace`
  实体上，不引入 Codex Project 作为容器。R4-01（#31）的研究据此收敛，不必再讨论实体归属，
  只需处理"一个 repo 属于多个 Workspace"的语义。
- **对用户**：会同时看到 Codex 的 Projects 和 Weft 的 Workspaces。这是有意的——它们回答
  不同问题（"我的文件夹"与"这次交付涉及哪些仓库"）。产品文案应避免把二者写成同义词。
- **不产生任何 schema 变更**（#3 的退出条件之一）。

## 参考

- 实测数据：`~/.codex/.codex-global-state.json`、`~/.codex/state_5.sqlite`（2026-08-10，build 6321）
- Local Project 结构：`{ id, name, rootPaths: string[], createdAt, updatedAt }`，
  归一化函数 `rootPaths ?? (path == null ? [] : [path])` 表明单数 `path` 只是历史回退
- 官方文档：<https://learn.chatgpt.com/docs/projects.md>、
  <https://learn.chatgpt.com/docs/environments/git-worktrees.md>
- 兼容矩阵：[docs/compat/codex-builds.md](../compat/codex-builds.md)
