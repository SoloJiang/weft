# Product

## Register

product

## Users

需要在本地同时协调多个代码仓库、围绕 issue 拆解并推进复杂工程任务的开发者。用户在 Codex 中查看 workspace、仓库、issue、任务与看板，并在必要时进入原生线程与 lead 或 worker 协作。

## Product Purpose

把 Weft 的多仓库编排、issue 拆解、lead/worker、线程通信、worktree、workspace 与
kanban 做成 Codex Desktop 的**第三种一等模式**，与 ChatGPT（日常对话）和 Codex
（编码线程）并列。聊天、主题、语言和窗口仍是宿主的。成功状态是用户在同一个
模式开关里切换三种工作方式，而不是打开另一个应用或一块插件面板。

## Product Form

Weft 的产品形态是：**Codex Desktop 里与 ChatGPT / Codex 同级的第三种模式。**
不是插件、不是浮层、不是第二扇窗口、也不是套在 Codex 模式上的管理后台。

宿主顶栏已经有一个模式开关。当前发行版菜单是 **ChatGPT** 与 **Codex**（内部
对应 everyday-work / coding-agent）。Weft 成为该菜单的第三项，视觉等级、切换
方式和持久化与前两项相同。

### 三种模式各回答什么

| 模式 | 用户在问 | 侧栏 | 主区 |
|---|---|---|---|
| ChatGPT | 日常对话 | 原生会话列表 | 原生聊天 |
| Codex | 这条编码线程怎么推进 | 原生 Codex 线程列表 | 原生线程 |
| Weft | 这个 issue 跨仓库怎么拆、怎么并行、卡在哪 | workspace / issue / 任务导航 | 看板、仓库、issue 详情；开口说话时仍是原生线程 |

模式切换改变的是**能力范围**，不是 chrome 在不在。三种模式共用同一扇窗口、
同一套主题与语言、同一个模式开关。头部保持同一形状：搜索与通知槽位始终在，
只是 Weft 下换成 issue 搜索与编排收件箱，而不是突然空一块。

### 什么算“和 Work / Codex 一样”

1. **同一开关、同一等级。** 不另做侧栏开关、状态栏徽章或独立启动器入口来表达
   “现在在 Weft”。用户怎么进 ChatGPT / Codex，就怎么进 Weft。
2. **切走就走干净。** 离开 Weft 后，ChatGPT / Codex 的原生列表、搜索、活动入口
   完整恢复。Weft 不在另外两种模式里残留入口或角标。
3. **切进来只换范围。** Weft 下隐藏与日常聊天无关的原生会话列表，换上
   workspace / issue / 任务。线程对话仍用宿主原生视图——这是唯一不替换的
   原生主表面，对应 Codex 模式里“点开线程就是聊天”的同一件事。
4. **默认可以停在 Weft。** 启动默认模式可以是 Weft，和宿主记住上次停在
   ChatGPT 还是 Codex 是同一类产品行为。
5. **没有浏览器降级。** 产品只存在于 Desktop 第三种模式。模式开关挂不上、
   注入失败或宿主结构对不上时，Weft 不可用，走修复 / `doctor`，不把用户
   送到浏览器标签页，也不把 Weft 降级成 Codex 模式里的附加行。独立浏览器
   页只允许作为开发预览，不是产品面。

### Weft 模式独占与共享

**三种模式共享：** 窗口、主题、语言、模式开关、原生线程渲染器。

**仅 Weft：** workspace / issue / 任务 / 看板 / 仓库图、weftd 编排、Weft 搜索与
收件箱。

**仅 ChatGPT / Codex：** 各自的原生会话列表与日常 / 编码 chrome。Weft 不重做、
不镜像。

**明确不是：** 嵌在 Codex 模式里的一块看板；第二个 App；Weft 品牌壳；聊天客户端；
Codex Local Project 的别名。

### 对实现的约束

产品形态是“第三种模式”，实现可以借 Codex 原生底座来显示编码线程（进入 Weft
前先切到 Codex 底座），但用户看到的必须是三个并列项，不能是“Codex 里开了
Weft”。官方 plugin 目前没有模式级 contribution point，因此 Desktop 上的模式
项就是产品壳，不是可选适配器。

**失败策略是 fail-closed。** 模式开关、Codex 底座或主表面挂不上：不能进入
Weft，提示修复，保持 ChatGPT / Codex 可用。禁止两条假降级：打开浏览器当
Weft，或在 Codex 模式里塞一行入口冒充第三种模式。

实现分层与探针见
[`docs/specs/2026-08-16-weft-third-mode-design.md`](docs/specs/2026-08-16-weft-third-mode-design.md)
§8。协议 spike 仍在
[`docs/specs/2026-08-08-codex-desktop-migration-design.md`](docs/specs/2026-08-08-codex-desktop-migration-design.md)
§5–6、§9。Vite 顶层页只作开发预览，不是产品形态的一部分。

## Brand Personality

原生、克制、任务导向。界面应安静地服务工程工作流，不建立独立于 Codex 的品牌壳或视觉语言。

## Anti-references

- 不呈现成另一个 Weft 客户端、嵌在 Codex 模式里的附加面板、第二扇窗口，
  或“请到浏览器打开”的降级产品。
- 不向用户暴露 direction、bus 等内部实现概念。
- 不使用“在 Codex 中打开”等暗示当前体验不属于 Codex 的文案。
- 不重复 Codex 已有的主题、语言、聊天或模式能力。
- 不使用与 Codex 不一致的控件、图标、间距、颜色或交互模式。

## Design Principles

- 第三种模式：Weft 与 ChatGPT / Codex 同级，只换能力范围，不另做入口。
- Codex 原生体感优先：扩展能力融入宿主，产品边界尽量不可见。
- Sidebar 承载全局导航与“新建 issue”一级动作；创建表单在主工作区以弹窗呈现，
  与 Weft 一致要求标题和类型。
- 以 issue 和任务表达工作：direction 只作为内部编排实体存在。
- 创建 issue 后立即启动 Lead 并进入 Codex 原生对话，不要求预先录入仓库。
- Lead 拥有任务拆解权：任务只能由 lead 会话创建，用户界面不提供手工新建任务。
- Lead 创建任务后自动调度 Worker；任务状态由编排事件推进，用户只处理打开对话、
  继续处理和验收完成，不提供审批门槛、通用移动或任意改状态。
- 运行时健康状态默认不可见，只有具体失败需要进入用户界面。
- 编排与聊天分层：Weft 模式管理工程上下文，开口说话仍用宿主原生线程。
- 复杂度渐进披露：默认界面只呈现完成当前动作所需的信息。
- 跟随宿主：主题、语言、模式入口和通用交互尽量复用 Codex。

## Accessibility & Inclusion

键盘可达性、可见焦点、明暗主题对比度和系统语言适配与 Codex 宿主保持一致。**宿主是基准，不是下限**：不因为觉得某个宿主值不够好就单方面调高，那会让 Weft 在宿主里显得是另一个应用。已知代价是焦点环沿用宿主的低对比度中性色，弱于常见无障碍基线——这是有意接受的取舍，要推翻先改这一节。

不依赖宿主取值的部分仍须自己守住：动效尊重 reduced-motion，状态不能只依赖颜色或无文本图标表达，交互元素必须可由键盘到达。
