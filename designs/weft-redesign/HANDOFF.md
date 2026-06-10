# Weft · LOOM 落地 handoff

> ⚠️ **部分内容已过时(superseded,2026-06-10)**:本稿写于 PTY/xterm 内嵌终端形态时期。该形态已整体移除(commit `2d26038`),全部会话改走 headless chat 引擎(`src-tauri/src/lead_chat/`),会话界面为 weft 自有 chat 时间线。文中涉及 PTY/xterm/合帧/终端面板的部分仅作历史参考,以 `ARCHITECTURE.md` 现行版为准。

把 `designs/weft-redesign/` 的 LOOM 设计提案落进真实 `src/`。原型是 React+Babel 演示,**不直接搬运**;本文件给的是 token 映射 + 组件清单 + 重构顺序。

> 关键利好:你现有架构是 Tailwind `@theme` → `--c-*` 每主题变量,组件全用 `bg-surface` / `text-ink` / `bg-brand` 这类语义类。**所以 LOOM 改色 = 只换 `--c-*` 的值**,`@theme` 映射和组件 className 一律不动,整库自动换肤。先做这一步就能拿到 80% 的视觉差异,且零风险。

---

## 1. Token 迁移(改 `src/index.css` 的 `--c-*` 值)

核心:基底 hue `292`(紫)→ `65/75`(暖石墨);brand `indigo` → **teal「经线 warp」**(结构/主操作);accent `orange` → **coral「纬线 weft」**(收束/交付)。变量名保持不变。

### 暗色(`:root`)

| 变量 | 现值 | → LOOM 新值 |
|---|---|---|
| `--c-bg` | `oklch(0.165 0.012 292)` | `oklch(0.175 0.008 65)` |
| `--c-surface` | `oklch(0.205 0.013 292)` | `oklch(0.214 0.009 65)` |
| `--c-raised` | `oklch(0.245 0.014 292)` | `oklch(0.252 0.010 65)` |
| `--c-border` | `oklch(0.30 0.014 292)` | `oklch(0.315 0.010 65)` |
| `--c-border-strong` | `oklch(0.37 0.016 292)` | `oklch(0.420 0.013 65)` |
| `--c-hover` | `oklch(0.29 0.014 292)` | `oklch(0.288 0.010 65)` |
| `--c-ink` | `oklch(0.96 0.005 292)` | `oklch(0.952 0.006 75)` |
| `--c-ink-muted` | `oklch(0.76 0.01 292)` | `oklch(0.745 0.012 75)` |
| `--c-ink-faint` | `oklch(0.62 0.012 292)` | `oklch(0.605 0.012 75)` |
| `--c-brand` (warp 填充) | `oklch(0.62 0.2 277)` | `oklch(0.575 0.105 212)` |
| `--c-brand-press` | `oklch(0.56 0.2 277)` | `oklch(0.525 0.100 214)` |
| `--c-brand-ink` | `oklch(0.99 0.005 277)` | `oklch(0.985 0.006 212)` |
| `--c-brand-ghost` | `oklch(0.62 0.2 277 / 0.18)` | `oklch(0.745 0.105 202 / 0.15)` |
| `--c-accent` (weft) | `oklch(0.70 0.18 38)` | `oklch(0.715 0.155 40)` |
| `--c-accent-ghost` | `oklch(0.70 0.18 38 / 0.16)` | `oklch(0.715 0.155 40 / 0.16)` |
| `--c-running` | `oklch(0.73 0.16 150)` | `oklch(0.770 0.150 158)` |
| `--c-waiting` | `oklch(0.80 0.13 80)` | `oklch(0.815 0.120 88)` |
| `--c-approval` | `oklch(0.74 0.17 45)` | `oklch(0.745 0.150 52)` |
| `--c-inject` | `oklch(0.72 0.12 215)` | `oklch(0.745 0.100 220)` |
| `--c-idle` | `oklch(0.64 0.015 292)` | `oklch(0.620 0.012 65)` |
| `--c-danger` | `oklch(0.64 0.2 25)` | `oklch(0.655 0.200 25)` |

### 浅色(`:root[data-theme="light"]`)

暖纸基底,brand/accent 加深保 AA:
`--c-bg oklch(0.967 0.006 75)` · `--c-surface oklch(0.992 0.004 75)` · `--c-raised oklch(1 0 0)` ·
`--c-border oklch(0.885 0.008 75)` · `--c-border-strong oklch(0.790 0.012 75)` · `--c-hover oklch(0.945 0.006 75)` ·
`--c-ink oklch(0.275 0.020 75)` · `--c-ink-muted oklch(0.455 0.018 75)` · `--c-ink-faint oklch(0.575 0.016 75)` ·
`--c-brand oklch(0.520 0.120 214)` · `--c-brand-ghost oklch(0.560 0.120 212 / 0.12)` ·
`--c-accent oklch(0.585 0.175 38)` · `--c-running oklch(0.580 0.150 158)` · `--c-waiting oklch(0.620 0.130 75)` ·
`--c-inject oklch(0.560 0.110 222)` · `--c-idle oklch(0.600 0.012 75)` · `--c-danger oklch(0.560 0.200 27)`。

### 建议新增的语义角色(小幅扩展,非必须)
- `--c-warp-line: oklch(0.745 0.105 202 / 0.4)` —— 比 brand 填充更亮的青,用于**激活指示条 / 链接 / 编织线**(看板 active、nav 2px 指示、scope/依赖图的线)。LOOM 里 brand 填充用深青、线/激活用亮青,两者分开读更清楚。
- 字体:DESIGN.md 已锁 **Geist / Geist Mono**。当前 `--font-sans` 还是 system stack;落地时换成 Geist(`@fontsource/geist-sans` + `geist-mono`,见原型 `styles.css` 头部的 `@import`),CJK 回退 PingFang / Noto。

---

## 2. 术语 i18n(改 `src/i18n/en.ts` + `zh.ts`)

已定决策(见项目记忆 `weft-issue-terminology` / `chinese-ui-copy-native`):

| 概念 | 旧 | 新 |
|---|---|---|
| 顶层工作单元 | thread / 工作线 | **issue**(带 `#编号`) |
| 其下执行单元 | direction / 方向 | **子任务** |
| Lead 主界面 | (控制塔) | **控制台** |
| 异常队列 | 需要你 | **待你处理** |
| 消息总线 | 总线 / bus | **Agent 协作** |
| 机制黑话 | 物化 / profile / provenance / 纵览 | **建副本 / 盘点 / 溯源 / 查看** |

保留开发者母语英文词:`scope` / `brief` / `worker` / `lead` / `curator` / `PR` / `diff` / `PTY`。
**内部状态枚举仍英文**(`waiting-approval` 等),仅 UI 映射成中文标签 —— 与现有 i18n 架构一致。

---

## 3. 组件清单(现有 `src/` 文件 → LOOM 改造)

| 现有文件 | LOOM 对应 | 改造要点 |
|---|---|---|
| `src/index.css` | 设计系统 | 换 `--c-*`(§1);加 `.issue-num`、weave 线、status pulse |
| `src/nav/WorkspaceNav.tsx` | 左栏 | `工作线`→`issues`;线条目带 `#编号`;`控制台` 导航;ws-switch 点击进首用流 |
| `src/session/LeadTab.tsx` + 主壳 | **控制台(home)** | 升为**默认界面**:对话流里出结构化卡(scope/派发/升级);右栏挂「本 issue·子任务」信任 mini-board + Agent 协作 |
| `src/board/WorkspaceHome.tsx` / `WorkspaceKanban.tsx` | 工作区看板 | 两级;新增**按仓 swimlane** 暴露热点仓(跨 issue 争用) |
| `src/board/ThreadBoard.tsx` | issue 看板 | 卡 → **信任凭证**:acceptance 信号(tests x/y·契约·review)+ 可展开溯源;去掉「拖卡为主」,改自动流转 + 动作 |
| `src/board/NeedsYouView.tsx` | 待你处理 | 升为**常驻 dock**,仅在工作界面显示(见 §4);保留全量 view |
| `src/session/SessionView.tsx` `Transcript` `DiffPanel` `DiffView` | 会话工作台 | 终端 focus 环 + 审批条 + 注入 banner + 按仓 diff + 受保护合并 + Inspect 逃生舱 |
| `src/board/RepoMapView.tsx` `RepoGraph.tsx` | 仓库地图 | 依赖图(青色边)+ 仓 profile + 热点仓标注 |
| `src/components/ui/*` (Button/StatusChip/Select/Input/Dialog) | 原子 | 随 token 自动换肤;StatusChip 对齐 §状态语义;信任 `Signal` 为新原子 |
| `src/i18n/{en,zh}.ts` | 文案 | §2 术语全量替换 |
| — 新增 — | 首用流 / 设置·有效配置预览 | 原型 `screen-onboarding.jsx` / `screen-settings.jsx` 为规格 |
| — 新增 — | 弹窗体系 | `dialogs.jsx`:新建 issue / 添加仓库 / **确认合并(不可逆 gate)** / 删除 issue(清理工作副本)。对齐 `src` 的 `dialogs.tsx` / `SettingsDialog` |
| — 新增 — | Toast / 通知 | `toasts.jsx`:成功/信息/警告/错误四类 + 可选撤销;接动作(合并/创建/删除/允许)。落地时配 OS 通知(Weft 后台时 needs-you) |
| — 新增 — | 错误边界 | 界面崩溃 → 可读 fallback + 逃生舱(重试 / 复制错误 / 开终端),绝不白屏(`App.jsx` 的 `ErrorBoundary`) |
| — 规格 — | 状态与边界 | 原型 `screen-states.jsx` = 空/加载/错误/边界的实现参照 |
| — 规格 — | 会话状态机 | `screen-session.jsx`:7 态(连接/运行/待输入/待审批/注入/暂停/退出)+ §4.3 键位归属 + 注入仲裁 |

---

## 4. 信息架构变更(LOOM 的 6 个结构动作)

1. **以对话为家**:默认界面 = 控制台(LeadTab 提升),看板退为同伴视图。
2. **看板 → 信任仪表盘**:卡带 acceptance 信号 + 溯源;自动流转,人只 批准/回答/打开/评审/合并。
3. **待你处理常驻 + 收敛**:dock 置顶,**仅工作界面显示**(控制台/Scope/看板/会话);设置/状态/设计提案/首用流不显示 —— 那里没有交付流可监督。空态 = 「自动流转中」。
4. **核心 wow 拍成电影**:Task → 编织式 scope 地图(写/只读/不涉及)→ 依赖顺序 → 唯一人工 gate。
5. **跨 issue 全局视角**:按仓 swimlane 暴露热点仓(竞品画不出)。
6. **机制隐入 Inspect**:worktree/PTY/MCP/add-dir 进逃生舱;失败可读 + 就地真路径/开终端。

**导航栏规则(统一后,只保留必需)**:① 顶栏(全局:面包屑 + ⌘K + 语言 + 主题)恒显;② 待你处理 dock 仅工作界面;③ 页头 scr-head 仅在有真实操作时出现(Scope/看板/会话/仓库地图),纯内容页(设置/状态/设计提案)靠面包屑作标题。

---

## 5. 重构顺序(低风险 → 高风险,可逐 PR 合)

- **M-A · 换肤**:`--c-*` 值替换(§1)+ Geist 字体 + `.issue-num`/weave。纯 CSS,整库自动变样,**当天可见**。
- **M-B · 术语**:i18n thread→issue / 方向→子任务 + issue 编号(§2)。
- **M-C · IA 重心**:控制台升为 home;待你处理 dock 收敛到工作界面;导航栏统一(§4)。
- **M-D · 信任仪表盘**:ThreadBoard 卡改 acceptance 信号 + 溯源;工作区加按仓 swimlane。
- **M-E · 会话 + wow**:会话工作台框架打磨;独立 Scope 拆解界面。
- **M-F · 新模块**:首用流 + 设置/有效配置预览;按「状态与边界」补齐空/加载/错误态。

---

## 6. 落地校验

- 换肤后逐一核对 AA 对比度(ink/ink-muted/ink-faint 在 surface 上 ≥ 4.5:1;status 配字形+标签,不单独靠色)。
- 嵌入终端在明暗两主题下**始终深色**(TUI 假设深色)。
- `prefers-reduced-motion`:pulse 停、滑动变即时(原型 `styles.css` 已含该分支,可直接搬规则)。
- 原型所有界面的具体间距/层级/动效时长可直接参照 `designs/weft-redesign/styles.css`(token + 组件类齐全)。
- **响应式(窄窗)**:< ~1100px 会话 diff 堆叠到终端下方(不再并排挤压);< ~940px 仓库地图改单列、顶栏收起搜索文案。Tauri 窗口可拖窄,需保这套降级。
- **嵌入终端 viewport(`.term`)固定深色**(已实现:覆盖 dark token),不随明亮主题翻白;外框 chrome 随主题。

---

## 7. 打磨规范(本轮确立 · 简洁 + 丝滑 · 落地时逐条核对)

落地 `src/` 时把以下当成 lint 清单,任何界面新增/改动都过一遍。

### 7.1 导航栏层级(只保留必需)
- **顶栏**(全局):面包屑 + ⌘K + 语言 + 主题 + 收起侧栏 —— 恒显,是唯一的窗口级 chrome。
- **待你处理 dock**:仅在**工作界面**(控制台 / Scope / 看板 / 会话)出现;设置 / 状态 / 设计提案 / 仓库地图 / 首用流不显示——那里没有交付流可监督,显示即噪音。空态读作「自动流转中」。
- **页头(scr-head)**:仅当页面有**真实操作/上下文**时出现(Scope 重新拆解、看板视图切换、会话 tool/branch/Inspect、仓库地图 添加仓库)。纯内容页(设置 / 状态 / 设计提案)**不加标题 bar**,靠面包屑作标题。
- **侧栏可收起**:顶栏最左常驻「收起/展开」;收起时左栏平滑收到 0、内容铺满。

### 7.2 去重(同一信息只在最合适处出现一次)
- 面包屑已含 issue 标题时,正文不再重复(反之亦然);本轮:控制台/看板面包屑去掉 `#编号`(正文/分段控件已展示)。
- 页头标题不重复正文里已有的卡(Scope 的任务标题只在「输入的 Task」卡;页头只留 eyebrow)。
- 持久侧栏不镜像对话流里的卡(控制台右栏不再列子任务,改进度 roll-up;子任务详情在派发卡 + 看板)。
- eyebrow 不重复面包屑(仓库地图 eyebrow 去「仓库地图」,留「CURATOR 维护」)。
- **不算重复**:同一异常出现在 dock(聚合)+ 卡(本卡状态)+ 对话(升级)——异常该在相关处都可见。

### 7.3 卡片左对齐(易踩的坑)
- `<button>` 默认 `text-align: center`。**所有卡片/列表/菜单类按钮必须显式 `text-align: left`**(nav 项、issue 行、看板卡、命令面板项、swimlane 卡、派发行…),否则带 `flex:1`/`grow` 的文本会在容器里居中,编号/标题左缘对不齐。仅纯动作按钮(`.btn`、分段 tab)保持居中。

### 7.4 动效统一(丝滑)
- 一套 token:`--t-fast .12s`(色彩/hover/focus)· `--t-mid .18s`(位移/展开/popover)· `--t-slow .24s`(切屏/大块);统一 `ease-out-expo`,**无 bounce**。
- 可点卡片:hover `translateY(-1px)` 微抬升,按下回落。
- 切屏:淡入 + 微位移(`.24s`),不硬切。
- 每条动效都有 `prefers-reduced-motion: reduce` 降级路径(pulse 停、位移变即时)。

### 7.5 平表面(简洁)
- 靠 **边框 + 一级表面抬升** 区分层级,**不叠装饰阴影**;阴影只留给真正悬浮层(popover / 命令面板 / toast,≤ 8px)。
- 表面层级:`bg < surface < raised`,`sunken` 用于终端/diff 井。

### 7.6 术语 & 文案(见 §2)
- issue / 子任务 / 控制台 / 待你处理 / Agent 协作;中文本地化、保留开发者英文术语;内部枚举仍英文。
