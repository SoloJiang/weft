# Weft 第三种模式：最终设计（2026-08-16）

地位：产品形态与 Desktop 壳层的准绳。产品未上线，本文推翻 08-08 里为
「浏览器一等降级面」服务的壳层假设。

编排、bus、curator、app-server 的已验证协议经验仍以
[`2026-08-08-codex-desktop-migration-design.md`](./2026-08-08-codex-desktop-migration-design.md)
§5–6、§9 为准，不在本文重复 spike 记录。

用户可见形态以 [`PRODUCT.md`](../../PRODUCT.md) 为准。像素以 [`DESIGN.md`](../../DESIGN.md)
为准。Workspace 与 Codex Local Project 的关系以
[ADR 0001](../adr/0001-codex-project-vs-weft-workspace.md) 为准。同文档壳以
[ADR 0002](../adr/0002-same-document-shell.md) 为准。

阅读顺序：`PRODUCT.md` → 本文 → `DESIGN.md` → 08-08 §5–6 / §9 → 两个 ADR。

## 1. 产品形态

Weft 是 Codex Desktop 顶栏模式开关里的第三项，与 **ChatGPT**（日常对话）和
**Codex**（编码线程）同级。不是插件、浮层、第二扇窗口，也不是套在 Codex
模式上的管理后台。

| 模式 | 用户在问 | 侧栏 | 主区 |
|---|---|---|---|
| ChatGPT | 日常对话 | 原生会话列表 | 原生聊天 |
| Codex | 这条编码线程怎么推进 | 原生 Codex 线程 | 原生线程 |
| Weft | 这个 issue 跨仓库怎么拆、怎么并行、卡在哪 | workspace / issue / 任务 | 看板、仓库、详情；开口仍是原生线程 |

约束（不可再议，要推翻先改 `PRODUCT.md`）：

1. 同一开关、同一等级。不另做入口表达「现在在 Weft」。
2. 切走就走干净。ChatGPT / Codex 不留 Weft 入口或角标。
3. 切进来只换范围。藏日常会话列表，换上 issue 导航。聊天是唯一不替换的原生主表面。
4. 可以默认停在 Weft。启动默认与上次离开时的模式写入
   `~/.weft-codex/desktop-host.json`，缺省 `weft`。
5. **没有浏览器降级。** 产品只存在于 Desktop 第三种模式。失败走修复 / `doctor`。
   独立浏览器页只允许开发预览，不是产品面。
6. 失败策略 **fail-closed**：模式开关、Codex 底座或主表面挂不上，则不能进入
   Weft。禁止两条假降级——打开浏览器当 Weft，或在 Codex 模式里塞一行入口。

三种模式共享：窗口、主题、语言、模式开关、原生线程渲染器。

仅 Weft：workspace / issue / 任务 / 看板 / 仓库图、weftd 编排、Weft 搜索与收件箱。

明确不是：嵌在 Codex 模式里的一块看板；第二个 App；Weft 品牌壳；聊天客户端；
Codex Local Project 的别名。

## 2. 目标与非目标

In：

- 多仓库 workspace、录入、curator 画像与 repo map
- issue → lead + 多任务（内部 `direction`）的编排
- lead / worker 都是真实 Codex 线程，聊天原生渲染
- bus、worktree、验收 / 继续 / 打开对话
- Weft 模式下的 sidebar、看板、仓库、issue 详情、搜索、收件箱

Out：

- 浏览器作为产品面或降级面
- 产品 OOPIF（sidebar / workspace / modal 三个跨源 iframe）
- 审批流 / Ask 桥（异常 approval 由 weftd 拒绝并记录）
- 多引擎、自建聊天 UI、Needs You 治理页
- Codex Local Project 1:1 映射（ADR 0001）
- 把 Weft 做成通用 Codex 插件 SDK（那是 Explodex 的事）
- Dashboard、甘特图、通用 Workflow Builder、周期任务

## 3. 总体架构

```
weft-codex（无窗口命令）
├─ launcher     专用 profile 启动官方 Codex + weftd；CDP 注入壳
├─ weftd        编排真相：store / orchestrator / curator / bus / git
└─ Codex Desktop（不改安装包、不破签名）
    ├─ 模式开关  ChatGPT | Codex | Weft
    ├─ Weft 壳   同文档一份 React 树，三个 shadow root
    └─ 原生线程  lead / worker 开口时的唯一主表面
```

三层职责：

| 层 | 拥有 | 不拥有 |
|---|---|---|
| weftd | workspace / issue / 任务 / binding / bus / worktree / 状态机 | 像素、模式菜单、DOM |
| launcher + renderer agent | 模式开关、锚点、挂载、打开线程、文件夹选择、头部槽位 | 业务状态 |
| React UI | Weft 范围内的展示与写操作（经 weftd API） | 宿主 chrome、聊天 transcript |

关键性质：

- 业务逻辑在 weftd，对 Codex 升级免疫。
- Desktop 壳是产品壳，不是可选适配器。官方 plugin 没有模式级 contribution。
- 每个任务对应「一个 Primary Codex 线程 + 一个 worktree」。fork 只新增
  `thread_binding`，不替换 Primary。
- 数据在 `~/.weft-codex`，fresh schema，不读旧 Weft。
- 专用 Codex profile，不改用户日常 `CODEX_HOME`。

## 4. 模式状态机

内部三个值：`work` | `codex` | `weft`。宿主菜单当前文案是 ChatGPT / Codex /
Weft；`work` 对应 everyday-work / ChatGPT。

```
启动
  ├─ --safe-mode → 只开官方 Codex；不启 weftd、不注入
  ├─ 探针失败（§8.5 必过项）→ 不可用
  │     官方 Codex 可开，doctor 说明哪条探针坏了
  └─ 探针通过
        ├─ 偏好 weft（默认）→ 先切 Codex 底座 → 进入 Weft
        ├─ 偏好 work / codex → 只注入菜单第三项，不挂 Weft 壳
        └─ 用户点 Weft → 先切 Codex 底座 → 进入 Weft

Weft 内
  ├─ view = workspace   主区 root 可见，渲染看板 / 仓库 / 详情
  └─ view = thread      主区 root 卸内容并隐藏；原生线程露出来
                        侧栏仍是 Weft 导航并高亮该线程

离开 Weft
  → 卸 React 树、摘三个 root、恢复原生列表与头部、恢复模式按钮文案
  → 写回 desktop-host.json
```

进入 Weft 必须先落到 Codex 底座：只有这条原生表面能打开编码线程。用户看到的
仍是三个并列项，不能写成「Codex 里开了 Weft」。

**没有 Tier 1 / additive 产品路径。** 探针分类里可以暂时留 `additive` 这个词
做调试日志，不能对用户表现为 Codex 模式里多了一行 Weft，也不能作为
「减法失败后的可用产品」。减法失败 = 不能进 Weft。

## 5. 数据模型

沿用已落地 schema，对外语言与内部实体分开。

| 用户语言 | 内部 | 要点 |
|---|---|---|
| Workspace | `workspace` | repo 集合的权威。不映射 Codex Project |
| 仓库 | `repo_ref` + `repo_profile` | 路径、base_ref、画像、关系 |
| Issue | `issue` | kind、`lead_codex_thread_id`、`lead_attention` |
| 任务 | `direction` | 四态 queued / working / review / done；`codex_thread_id` |
| — | `thread_binding` | 每个原生线程一行；fork 不替换 Primary |
| — | `bus_message` | durable inbox + 活动日志 |
| — | `worktree` | 按任务幂等 |

`thread_binding`：`issue_id`、可空 `direction_id`、`parent_thread_id`、
`root_thread_id`、展示标题、`is_primary`。
`issue.lead_codex_thread_id` / `direction.codex_thread_id` 仍是 canonical
Primary 指针。打开数据库时把旧指针幂等回填到该表。

状态推进：

- 任务启动（plan+impl 与 impl-only 相同）→ `working`
- turn 进行中 → `working`
- turn 完成 → `review`
- 用户验收 → `done`（同时清 attention）
- 启动失败 / turn 失败 / quota → attention
- 「继续处理」发补充要求并回到 `working`

`mandate` 仍区分 plan+impl / impl-only（brief 与 `task_create`），只影响
工人怎么写，不单独占一列。旧库里的 `planning` 在打开数据库时并入 `working`。

Lead 拥有 `task_create`。UI 不提供手工新建任务。Issue 可先于仓库创建。

未知原生线程的归属：先查 `thread_binding`；若无，用元数据级 `thread/read`
读明确的 `forkedFromId`，有界向上追溯到已知祖先后写回整条链。无已知祖先的
普通 Codex 线程保持 unbound，不猜测。UI 走 `POST /api/threads/resolve`：
解析中显示「正在关联」；重试耗尽仍无 binding 则说明这条对话尚未关联到
Weft issue。

## 6. 编排（不变的内核）

weftd 经 app-server 创建与驱动线程。细节与已验证语义见 08-08 spec §5–6、§9。
本文只锁产品层约定：

- Lead：cwd = issue 主仓或 workspace 根；brief 含仓库 id / 基线分支 / 工具说明。
- Worker：materialize worktree → 建线程（cwd = worktree）→ mandate brief。
  `task_create` 后立即入自动调度队列；重启补调度无线程的 queued 任务。
- 沙箱：`approvalPolicy=never`；Lead read-only，Worker workspace-write。
  异常 approval 协议级拒绝，无审批 UI。
- Bus：身份从 MCP URL path 派生。空闲 `turn/start`，活跃 `turn/steer`。
  **活跃期间 `turn/start` 会被静默丢弃。**
- 人打开原生线程 = 接管；weftd 对该线程转只观察。投递靠启动确认，不靠跨进程
  turn 通知。
- 全部 weft 线程 `threadSource: "weft-codex"`。
- curator 分析是 ephemeral `thread/start`，不进 Desktop 线程列表。
- 录入：原生多目录选择 → `POST /api/workspaces/{id}/repos/import`（最多 64
  条，逐项成败，不整批回滚）。`base_ref` 按 origin/HEAD → main/master →
  当前非标准分支 → HEAD 识别。

## 7. 为什么改壳

08-08 用三个跨源 iframe（sidebar / workspace / modal）是为了同一份 UI 能在
浏览器里当一等产品跑。产品不再需要那条路。

iframe 带来的税，全部不再付：

- CSS 变量不跨 frame，必须 `weft:host-context` token 信封
- Chromium 给透明 iframe 涂 `color-scheme` 黑底
- `frame-src` 拦 loopback，逼出持久 CSP bypass
- `BroadcastChannel` + `bridge_id` 同步两侧状态
- `ModalApp` 第三棵树，dialog 状态要 host 代持并回放
- 三套 `surface=` 外壳

社区同类（Explodex / Codex++）也是 CDP + `data-*`，没有谁把「浏览器降级」
做成产品。Weft 比它们更彻底：占满第三种模式，而不是往 Codex 模式里加一块。

## 8. Desktop 壳

### 8.1 同文档、一份 React 树

renderer agent 在宿主里放三个挂载点，各 `attachShadow({ mode: "open" })`：

| id | 角色 |
|---|---|
| `#weft-codex-sidebar-root` | 侧栏滚动区 |
| `#weft-codex-workspace-root` | 可见、可交互、非 inert 的主区 |
| `#weft-codex-overlay-root` | `position: fixed; inset: 0`；平时 `pointer-events: none` |

**一份 React 树。** `createRoot` 建在 overlay shadow 的宿主节点上，侧栏与主区
用 `createPortal` 送进另外两个 shadow。workspace 选择、看板路由、搜索 / 收件箱
面板、dialog 状态都是普通 React state，不再有 `BroadcastChannel`。

Radix Dialog / Select 的 portal 必须指定到 overlay shadow，禁止 port 到宿主
`document.body`。

Vite 增加 library 入口（IIFE，方便 CDP 注入），导出：

```ts
mountWeft(options: {
  sidebar: ShadowRoot
  main: ShadowRoot
  overlay: ShadowRoot
  host: WeftHost
}): () => void
```

返回值卸树。宿主导航或 React 重绘拆掉 root 时，agent 先调用卸树再挂，单实例。
全局键 `__weftCodexAgentV1` 的旧实例必须先 `dispose`。

不再有产品路径：`surface=sidebar|workspace|modal|standalone`、`bridge_id`、
`host_origin` 查询参数、`weft:host-context` / `weft:host-action` /
`weft:dialog-state` postMessage、`ModalApp`。

开发预览：Vite 顶层页挂同一份组件，mock 一个 `WeftHost`，`/api` 走 dev
proxy。不走 shadow，不走 CDP。那不是产品面，发行说明里不出现。

### 8.2 WeftHost

agent 在调用 `mountWeft` 之前把宿主能力装成同一文档上的对象。不是
`postMessage` 协议。

```ts
interface WeftHost {
  readonly locale: string
  readonly view: "workspace" | "thread"
  readonly threadId?: string
  readonly weftdOrigin: string
  readonly headerActions: "native" | "fallback"
  openThread(threadId: string): Promise<void>
  showWorkspace(): void
  pickRepositories(): Promise<string[]>
  setInboxCount(count: number): void
  onCommand(handler: (command: "search.open" | "inbox.open") => void): () => void
  onView(handler: (view: "workspace" | "thread", threadId?: string) => void): () => void
}
```

规则：

- `locale` 读宿主 `documentElement.lang`。UI 不猜 `navigator.language`。
- **没有 `theme` / `tokens` / `projectId` / `cspBypass` 字段。** 主题靠
  shadow 继承。`projectId` 按 ADR 0001 不存在。CSP 诊断只进 CLI。
- `weftdOrigin` 是 loopback 绝对地址（默认 `http://127.0.0.1:47810`）。
  同文档挂载后页面 origin 是宿主（`app://-` 一类），相对路径 `/api` 会打到
  Codex，不是 weftd。所有 `fetch` / `EventSource` 必须拼在这个 origin 上。
- `openThread` 按 `data-app-action-sidebar-thread-id` 点原生行，退避
  `[0, 80, 160, 320, 640, 1000, 1800]` ms。禁止 `location.assign(codex://…)`
  当专用 profile 的主路径（会落到另一个 Codex 实例）。等待期间 UI 显示
  「正在打开」；最终失败可重试，不静默吞点击。
- `showWorkspace` 把 `view` 设回 workspace，主区 root 重新可见。
- `pickRepositories` 走 macOS 原生文件夹对话框。React 不拿文件系统权限。
- `setInboxCount` 只画头部角标。agent 不读原生铃铛状态。
- `headerActions` 是 `native`（模式行槽位在，搜索 / 收件箱占原位）或
  `fallback`（改画在 Weft 侧栏顶部）。槽位后出现时 agent 改
  `data-weft-codex-header-actions`，React 跟着改。
- `onCommand` / `onView` 返回取消订阅。agent 在卸树时必须取消。

### 8.3 CSS、token、shadow

宿主 CSS 变量会穿进 shadow，所以 `--color-token-*` / `--vscode-*` /
`--font-*` 自动继承。宿主的 `button` / `input` 元素规则不会打进来；我们的
preflight 也不会污染宿主。

圆角例外：宿主 `--radius-*` 常以 rem 计，按 16px 根字号解释；Weft 根字号是
13px（`DESIGN.md`）。agent 用 `getComputedStyle` 把允许列表里的半径读成
**px**，写到三个 shadow host 上（`--r-lg` / `--r-md` / `--r-sm`）。颜色与
字体直接继承，不必再抄一遍白名单信封。

允许列表仍以发行包实测为准，与 `launcher/src/probes.ts` 的
`ALLOWED_CODEX_TOKENS` 对齐。缺核心表面 / 前景 / 字体则探针失败，不能进 Weft。

样式注入：library 构建产出一份 CSS。agent 用 constructable stylesheet
（或等价的 `<style>` 克隆）adopt 进三个 shadow。不要把 Weft 样式表挂到宿主
`document`。

视觉规则继续写在 `ui/src/index.css` 的 unlayered 层。不在组件上堆视觉
Tailwind。文本字段聚焦不画 outline，光标即信号；按钮、链接、Select、`summary`
仍是 `outline: 2px solid var(--focus)`。弹窗底是浮层表面（`--panel-2` /
dropdown），不是主表面。

### 8.4 锚点与探针

探针是产品能不能开的硬条件，不是兼容分档。定位只许 `data-*`、landmark、
角色。禁止 aria-label 文案、Tailwind class、React fiber、`Function.prototype`
补丁。

`main` 不得 `querySelector("main")`。Codex 路由会同时留一个 inert 旧 main。
只选 viewport 内、可见、可交互、不在 `inert` / `aria-hidden` / `hidden`
祖先下的最大语义 `main`（现有 `visibleMainRoute()`）。

| 探针 | 缺失时 |
|---|---|
| 主 renderer / `#root` | 不能进 Weft |
| 可见、可交互、非 inert 的 `main` | 不能进 Weft |
| `[data-app-action-sidebar-scroll]` | 不能进 Weft |
| 模式开关（`nav` 内唯一 menu 触发器，且有非空 `id`） | 不能进 Weft |
| 主题核心 token + `lang` | 不能进 Weft |
| titlebar drag region（可见 main 内） | 不能进 Weft（主区会与拖拽带重叠） |
| 线程行 `thread-id` / `thread-active`（**侧栏已有会话行时**） | 不能进 Weft |
| 线程行（侧栏 **零** 会话） | 记 N/A，**仍可进 Weft**。空 profile 不能永远锁死第三种模式 |
| 头部 action 槽 | 可选。搜索 / 收件箱改画在 Weft 侧栏顶部 |
| `sidebar.section` / `heading` / `projectCreate` | **不再作为进 Weft 的条件**。它们只服务已删除的 additive 行 |

`doctor` 文案不出现 CSS 选择器。选择器只进 `detail` 供排障。

### 8.5 模式菜单

在原生菜单里插入第三个 `menuitem`。只显示一个选中图标。进入 Weft 前先点原生
Codex 项落到编码底座，再切 Weft 壳。离开时恢复宿主 label / ARIA。

没有「自有 toggle」回退。菜单探针失败 = 不能进 Weft。

`--mode=work|codex|weft` 覆盖本次启动；之后的切换仍写回
`~/.weft-codex/desktop-host.json`。

### 8.6 头部槽位

Weft 下隐藏原生搜索与活动按钮，在同一位置放克隆按钮。点击只通知 React
打开搜索或收件箱面板。`/` 聚焦搜索，不抢 `⌘K` / `⇧⌘P` / `⌥⌘U`（宿主绑在
Electron 菜单 accelerator，renderer 拦不住）。

搜索：对当前 workspace 已拉取的 `/api/issues` 看板做客户端过滤。种类顺序
issue → 任务 → artifact → thread。不新增接口。

收件箱 = `lead_attention` + `direction.attention` + 待验收（`status=review`
且无 attention）+ `bus.undelivered`。
`bus.parked` 不进：人正在该线程上说话，turn 结束会自动 flush。Lead 停滞排在
任务前面。失败 turn 也落在 review，但已有 attention 行，不再重复。点 Lead /
失败任务且有线程时只记路由并开口，不 `showWorkspace()`；待验收打开 issue
详情，因为验收在 Weft 表面。

### 8.7 Weft 模式 UI

**侧栏只做导航。** workspace 切换、Kanban / 仓库入口、新建 issue、issue 列表。
一行一个点击目标，打开该 issue 的 Primary Lead；不放展开箭头，也**不在侧栏里画
会话树**——会话属于 issue 详情，理由见 `2026-08-16-host-container.md` §6。收件箱
里有条目的 issue 在主行标「需要你」，文案不只靠颜色。打开任何绑定线程时切到对应
workspace 并高亮该行。`view=workspace` 时即使原生 DOM 仍暂时标着 active thread，
也不能残留错误高亮。

主区 `view=workspace`：看板与 issue 详情是**同一块 stage 的两栏**，不是互相替换的
两个路由——点看板卡片时看板一直在（容器文档 §5.1）。详情里依次是会话、产物、
任务、活动：会话只列 Lead 的主会话与 fork，任务各自带自己的会话入口，不重复列。
仓库 / repo map 仍是整块视图。不重复侧栏的新建 issue 表单。详情不提供手工新建
任务。主区顶部避开 titlebar drag region。

主区 `view=thread`：不盖原生线程。侧栏仍在。

弹窗（新建 workspace / issue / 导入仓库 / 继续处理）走 overlay shadow 里的
Radix Dialog。

看板用户动作只有：打开对话、验收完成、继续处理。不拖拽、不任意改状态。

用户可见字符串只走 `ui/src/i18n/{en,zh}.ts`。语言跟宿主 `lang`。

不向用户暴露 `direction`、`bus`、兼容档、CSP bypass。

### 8.8 失败与修复

| 情况 | 行为 |
|---|---|
| §8.4 必过探针失败 | 不进入 Weft；`doctor` 列出失败探针 |
| weftd 起不来 | 不进入 Weft；Codex 可开 |
| 挂载后宿主重绘拆掉 root | 重挂，单实例 |
| Sparkle 改结构 | 下次启动探针失败，Weft 不可用，等适配 |
| `--safe-mode` | 不启 weftd、不注入；官方 Codex |
| `connect-src` / `script-src` 拦 loopback | 专用 profile 才启用 CSP bypass，先 enable 再 reload。不为 `frame-src` 开 bypass |
| bypass 之后仍不能连 weftd | 不能进 Weft |

用户界面不常驻「兼容模式」「CSP bypass」等诊断。那些只进 CLI。

## 9. Launcher 生命周期

无窗口命令 `weft-codex`：

1. 解析官方 `/Applications/ChatGPT.app`（bundle `com.openai.codex`）。
2. 起或复用 weftd（默认 `127.0.0.1:47810`，`WEFTD_ADDR` 可覆盖）。
3. 用独立 `CODEX_HOME` + loopback CDP 端口拉起官方 Codex。
4. 等主 renderer + hydration，跑 §8.4 探针。
5. document-start 安装 agent；需要 bypass 则先 enable 再 reload。
6. 按偏好进入 Weft 或只挂菜单第三项。
7. 把产品 skill 同步进该 profile 的 `$CODEX_HOME/skills`。
8. 退出：卸注入、关 bypass、回收自己拉起的 Codex 与 weftd。CDP 端口无应用层
   认证，存活期间只许本机可信代码。

子命令保留：`doctor`、`probe`、`attach`、`--once`、`--safe-mode`。
`weft-codex-host` 只是开发别名。

发行：Bun 编译的单个 macOS 可执行文件 + `weftd` + 构建后的 library 资产
（IIFE + CSS）。不发布第二个 `.app`。不重签官方 Codex。对外分发再加
Developer ID / notarization。

## 10. weftd HTTP

UI 只读这些写路径。SSE 只作失效提示，真值重新 GET。

| 方法 | 路径 | 作用 |
|---|---|---|
| GET/POST | `/api/workspaces` | 列表 / 新建 |
| GET/POST | `/api/ui-state` | 上次选中的 workspace |
| GET/POST | `/api/workspaces/{id}/repos` | 仓列表 / 单仓加 |
| POST | `/api/workspaces/{id}/repos/import` | 批量导入 |
| GET/POST | `/api/issues` | 看板 / 新建 issue（建完启 Lead） |
| POST | `/api/issues/{id}/spawn-lead` | 重试 Lead |
| POST | `/api/issues/{id}/message` | 给 Lead 发消息 |
| GET | `/api/issues/{id}/bus` | 活动日志 |
| POST | `/api/threads/resolve` | 原生线程 → binding |
| POST | `/api/issues/{id}/lead-thread` | 提升 Primary Lead |
| POST | `/api/directions/{id}/spawn` | 重试 Worker |
| POST | `/api/directions/{id}/message` | 继续处理 |
| POST | `/api/directions/{id}/complete` | 验收 |
| POST | `/api/directions/{id}/attention/clear` | 清 attention |
| POST | `/api/repos/{id}/analyze` | 单仓重试分析 |
| GET | `/api/repos/{id}/profile` | 画像 |
| POST | `/api/workspaces/{id}/analyze` | 工作区分析 |
| POST | `/api/workspaces/{id}/analyze-relations` | 只刷关系 |
| GET | `/api/workspaces/{id}/repo-map` | 地图 |
| * | `/api/issues/{id}/artifacts` 等 | 产物 |
| GET | `/api/events` | SSE |
| GET | `/healthz` | daemon 活着 |

同文档挂载后请求跨源。weftd 必须对 Desktop 宿主 origin 开 CORS，范围
`/api/*`、`/healthz` 与 `/web/*`（agent 用 `fetch` 读 library CSS）：

- 允许 origin：`app://-`（及实测到的宿主 origin）。开发预览继续同源，不靠 CORS。
- 方法：`GET` `POST` `OPTIONS`。
- 头：`content-type`。不带 cookie，不反射任意 origin。
- SSE 同样回 CORS 头。

MCP bus 仍是 `/bus/:thread/:dir/mcp`，身份在 path 上，不加 CORS（agent 不是
浏览器页）。

## 11. 安全

- 不改、不重签官方 app。专用 profile，CDP 只绑 loopback。
- launcher 退出：卸注入、关 bypass、回收自己拉起的进程。
- React 不拿文件系统权限；选仓只经 Host。
- Host API 是同文档对象，不是 `postMessage("*")`。
- 不包装成让 Codex 绕过自身 computer-use 限制的通道。
- 官方若出 `ui.sidebar` 或模式级 API：只换壳，不换 weftd。

## 12. 从当前实现迁到本文

允许删。顺序：

1. 文档：`PRODUCT.md` / AGENTS.md / 本文已是入口。08-08 降为协议 spike。
2. weftd：给 `/api` 与 `/healthz` 加 CORS。编排与 schema 不动。
3. UI：Vite library 入口 `mountWeft`。合并 `SidebarApp` / `App` / `ModalApp`
   为一棵树 + portal。`fetch` / SSE 改走 `host.weftdOrigin`。
4. 删除产品路径：`surface-channel.ts`、`ModalApp.tsx`、跨 frame
   `host-context` 信封、`surface=modal`、产品用 `standalone` 路由。
   Vite 顶层页只留开发 mock Host。
5. launcher：三个 root 改 shadow + `mountWeft`，去掉 iframe / handshake /
   dialog 代持。探针失败不再落到 additive 产品行。`projectId` 与
   `cspBypass` 不再进 Host API。
6. 兼容矩阵：历史 `additive` 行保留为当时实测；新采集把减法失败记成
   「不能进 Weft」，不再当产品档。

08-08 里已闭环的编排、bus、curator、线程可见性实测继续有效，不重做。

## 13. 风险

- **和发行版绑定。** 模式菜单或侧栏结构一变，Weft 整面停。对冲是硬探针、
  单实例重挂、`doctor` 说人话。没有标签页可逃。
- **shadow 与宿主焦点 / 字体。** 变量能继承，焦点环和 drag region 要在真机
  量。冲突则改 scoped 选择器，不退回 iframe。
- **跨源 API。** CORS + 可能的 `connect-src` bypass。比 `frame-src` 窄。
- **线程列表滞后。** 打开线程仍要有界等待语义行。Desktop 是否始终读
  `state_*.sqlite` 仍是社区未闭合问题。
- **空 profile。** 无线程行时不把第三种模式锁死；用户第一次从 Weft 建 Lead
  后，后续启动再校验线程锚点。
- **官方若出模式级 API。** 只换壳，不换 weftd。

## 14. 验收（上线前）

产品形态：

- 模式菜单恰好三项，Weft 与 ChatGPT / Codex 同级。
- 进入 Weft：侧栏是 workspace / issue，头部是 Weft 搜索与收件箱。
- 打开 Lead / 任务：主区是原生线程，侧栏仍在并高亮。
- 点看板返回：主区回到 Weft 编排页，不高亮一个已经不在看的线程。
- 离开 Weft：原生列表与头部完整恢复，无残留角标、无残留 root。
- 探针失败：进不了 Weft，没有「去浏览器」的产品文案，也没有 Codex 里的附加行。
- 空 profile：仍能进 Weft、建 issue、打开新 Lead。

编排（回归，不降标准）：

- 建 issue → Lead 线程出现并打开。
- Lead `task_create` → 自动 worktree + Worker。
- 空闲 `turn/start`、活跃 `turn/steer`。
- 验收 / 继续 / 打开对话。
- daemon 重启后 binding、watcher、未结算 bus 恢复。

视觉：

- Weft 表面用宿主 token，无独立色板。
- 文本字段聚焦无 outline；按钮与 Select 有 2px 焦点环。
- 弹窗底是浮层表面，不是纯黑主表面。
- 明暗与中/英跟宿主，无需 token 信封。

## 15. 相对 08-08 的决策对照

| 08-08 | 本文 |
|---|---|
| 浏览器是降级面 | 删除。开发预览不是产品 |
| 三 iframe + Host Context | 同文档一份 React 树 + shadow + `WeftHost` |
| 减法失败回 Tier 1 additive | 不能进 Weft |
| 自有 toggle 保 Weft 可达 | 删除。菜单挂不上即不可用 |
| `frame-src` CSP bypass | 不为 iframe 开。只可能为 `connect-src` / `script-src` |
| 收件箱 = attention + undelivered | 加上 `lead_attention` 与待验收；`parked` 仍不进 |
| `HostContextV1.projectId` | 不存在 |
| 相对路径 `/api` | `host.weftdOrigin` 绝对地址 + CORS |
| 编排 / bus / curator / binding | 不变 |
