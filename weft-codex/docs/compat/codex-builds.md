# Codex Desktop 构建兼容矩阵（2026-08-10 起）

本文件是 **N0-05（#6）CI 回归信号的数据源**，不是叙事文档。每次在新安装的
`/Applications/ChatGPT.app` 上重跑 launcher probe 或 `docs/spike-app-server/`
探针后，把结果**追加**为新行；**不要改写历史行**——历史行就是回归基线。CI 对
§2 做逐行 diff：同一 `anchor id` 在相邻 build 之间 `ok` 由 `true → false`，即判定
为兼容性回归。

## 0. 两条独立版本轴

这两条轴**互不隐含**，不要因为其中一条复验过就认为另一条也复验过。

- **Desktop bundle 轴**：`/Applications/ChatGPT.app` 的 `CFBundleShortVersionString`
  与 `CFBundleVersion`。影响 CDP 注入、DOM/CSS token 锚点、CSP / `frame-src`、
  `codex://` scheme、原生菜单结构。对应 §1 / §2 / §3。
- **app-server 协议轴**：`weftd` 经 `WEFT_CODEX_COMMAND`（默认 `codex`，见
  `crates/app-server/src/command.rs`）在 `$PATH` 上解析并 spawn 的独立 `codex`
  CLI / `app-server` 子进程版本，与 Desktop bundle 版本**不保证同步**。影响
  `turn/steer` vs `turn/start`、`threadSource` 落库、跨进程 turn 通知。对应 §4。

`weftd` 从不通过 ChatGPT.app 内嵌的二进制说话，四个 spike 脚本也都是
`create_subprocess_exec("codex", "app-server", ...)`。因此 Desktop build 从 6119
升到 6321，**并不自动**让协议结论失效，也**不自动**让它们重新生效。

## 1. 已验证 Build 一览

| Desktop 版本 | Build | Tier | 验证日期 | 验证方式 | 结果 |
|---|---|---|---|---|---|
| 26.727.51351 | 6119 | `weft-mode` | 2026-08-08 / 2026-08-09 | 真机 launcher probe + Stage 2–4 真机闭环 | **PASS**（见 spec §7.5 / §9 / §10） |
| 26.803.41515 | 6321 | `weft-mode` | 2026-08-10 | `node dist/cli.js probe --endpoint=… --target-url='app://-/index.html'`，专用 profile + `--safe-mode`（无注入、无 weftd）；CSP 与协议轴另测，见 §3 / §4 | **PASS**，但有 3 项假阳性/语义问题，见 §5 |
| 26.810.52044 | 6662 | `weft-mode` | 2026-08-16 | 同上（专用 profile + `--safe-mode`，`injected` 计数为 0）；CSP 与协议轴本次未复验，§3 / §4 仍停在 6321 | **初测 FAIL**：`safe-mode`，两个 token 别名消失；改读底层 `--vscode-*` 后 31 项全绿。见 §8 |

> Tier 取值与采集当时 `launcher/src/probes.ts` 的 `CompatibilityTier` 一致：
> `safe-mode` / `additive` / `weft-mode`。**从 2026-08-16 起，`additive` 不再是
> 产品档**（见 08-16 spec §4 / §8.4）：减法失败 = 不能进 Weft。本表历史行仍按
> 当时分类记录，不要改写。当前代码把 token / locale 记成 `base`（缺失则不能进
> Weft）；历史行里的 `additive` 与之同义，只是采集当时的词。
>
> 6321 这一行是在**未注入**的纯净原生 DOM 上采集的（`injection.mode === null`、
> 无 `weft-codex-*` 根节点与样式），因此测到的是宿主自身结构，不含 Weft 影响。

## 2. Desktop DOM / token 锚点矩阵

逐 `build × anchor` 一行（tidy 格式）：新增 build 只追加行，diff 天然落在单个交叉
点上。`anchor id` 与 `requiredFor` 直接取自 `launcher/src/probes.ts` 的 `SELECTORS`、
`TOKEN_PROBES` 与 `reportFromSnapshot`，**不要另造命名**。

`ok` 列取值：`true` / `false` / `未记录`。**`未记录` 不等于 `true`**——它表示当时没有
逐锚点留证，不能作为回归基线使用。

`requiredFor` 列记录**该次采集时生效的分级**。2026-08-10 的重分级（`projectCreate` /
`threadRow` / `threadRoute` 由 `optional` 升为 `subtractive`，并新增 `threadActive`）
只体现在 6321 行；6119 行保留当时的 `optional`，因为那才是当时的真实判定依据。

| Build | Anchor id | requiredFor | ok | 依据 / 实测 |
|---|---|---|---|---|
| 6119 | `renderer.root` | base | true | 由 tier 反推：达到 `weft-mode` 蕴含全部 base 通过 |
| 6119 | `renderer.main` | base | true | 同上（6119 未区分 inert 路由，见 §5） |
| 6119 | `sidebar.scroll` | base | true | spec L455 明确列出该语义属性 |
| 6119 | `sidebar.section` | base | true | spec L455 明确列出 |
| 6119 | `sidebar.heading` | additive | true | spec L455 明确列出 `section-heading` |
| 6119 | `sidebar.projectCreate` | optional | **未记录** | spec L455 只记录了 `project-row`，未记录 `project-create`（6321 已证实两者都存在，见 §5） |
| 6119 | `sidebar.threadRow` | optional | true | spec L455 明确列出 `thread-row` |
| 6119 | `sidebar.threadRoute` | optional | true | spec L465–472：deep-link 实测命中 `data-app-action-sidebar-thread-id` |
| 6119 | `mode.switcher` | subtractive | true | spec L330–334：原生菜单实测 ChatGPT / Codex，Weft 注入为第三项 |
| 6119 | `host.locale` | additive | true | 由 tier 反推 |
| 6119 | `titlebar.dragRegion` | optional | **未记录** | optional 探针失败不影响 tier，无法由 `weft-mode` 反推 |
| 6119 | `theme.*`（18 个 token） | additive | true | 由 tier 反推：任一 additive token 缺失会降到 `safe-mode` |
| 6321 | `renderer.root` | base | true | `#root` |
| 6321 | `renderer.main` | base | true | **假阳性风险**：文档中同时存在 2 个 `main`，见 §5 |
| 6321 | `sidebar.scroll` | base | true | n=1 |
| 6321 | `sidebar.section` | optional | true | n=2。2026-08-16 起不再作为进 Weft 条件（原 base，只服务已删除的 additive 行） |
| 6321 | `sidebar.heading` | optional | true | n=2。2026-08-16 起不再作为进 Weft 条件 |
| 6321 | `sidebar.projectCreate` | optional | true | n=1，属性确实存在（同时 `project-row` n=4）。2026-08-16 起不再作为进 Weft 条件 |
| 6321 | `sidebar.threadRow` | base | true | n=24。2026-08-16：有会话行时缺失则不能进 Weft |
| 6321 | `sidebar.threadRoute` | base | true | n=24 |
| 6321 | `sidebar.threadActive` | base | true | n=24；2026-08-10 新增探针，语义经真实点击验证（§5.1） |
| 6321 | `mode.switcher` | base | true | `nav` 作用域内、排除 sidebar 后恰好 1 个触发器（文档内共 9 个同形按钮）；触发器 `id` 为 `radix-_r_3_`（Radix 自动生成，非空）。2026-08-16：缺失则不能进 Weft |
| 6321 | `sidebar.headerActionSlot` | optional | true | 2026-08-11 新增探针。模式行恰好 1 个非模式按钮的元素子节点（`div.ms-auto flex items-center gap-1`），内含 Search 与 View activity 两个按钮。定 `optional` 的理由见 §5.9 |
| 6321 | `host.locale` | additive | true | `en-GB` |
| 6321 | `titlebar.dragRegion` | base | true | 文档内 3 个；`main[0]`（inert）内 1 个、`main[1]`（可见）内 2 个。探针已改为**可见 main 作用域**，实测 2 个。2026-08-16：缺失则不能进 Weft |
| 6321 | `theme.sidebarSurface` | additive | true | `#f2f2f3` |
| 6321 | `theme.mainSurface` | additive | true | `#fafafa` |
| 6321 | `theme.dropdownSurface` | additive | true | `rgb(251, 251, 251)` |
| 6321 | `theme.foreground` | additive | true | `#383a42` |
| 6321 | `theme.secondaryText` | additive | true | `color-mix(in srgb, #383a42 65%, transparent)` |
| 6321 | `theme.border` | additive | true | `rgba(56, 58, 66, 0.078)` |
| 6321 | `theme.borderHeavy` | additive | true | `rgba(56, 58, 66, 0.117)` |
| 6321 | `theme.primary` | additive | true | `#526fff`（6119 记录为 `#0169cc`，主色已变） |
| 6321 | `theme.buttonForeground` | additive | true | `#fafafa` |
| 6321 | `theme.linkForeground` | additive | true | `#526fff` |
| 6321 | `theme.inputBackground` | additive | true | `rgba(251, 251, 251, 0.96)` |
| 6321 | `theme.inputBorder` | additive | true | `rgba(56, 58, 66, 0.117)` |
| 6321 | `theme.hoverBackground` | additive | true | `rgba(56, 58, 66, 0.053)` |
| 6321 | `theme.fontSans` | additive | true | `Maple Mono NF CN, -apple-system, …`（受用户字体设置影响，非稳定基线） |
| 6321 | `theme.fontMono` | additive | true | `Maple Mono NF CN, ui-monospace, …`（同上） |
| 6321 | `theme.radiusLarge` | additive | true | `calc(.625rem * 1.25)` |
| 6321 | `theme.radiusMedium` | additive | true | `calc(.5rem * 1.25)` |
| 6321 | `theme.radiusSmall` | additive | true | `calc(.375rem * 1.25)` |
| 6662 | `renderer.root` | base | true | `#root` |
| 6662 | `renderer.main` | base | true | 文档内仍有 2 个 `main`（同 6321 的 inert 旧路由），可见路由解析成功 |
| 6662 | `sidebar.scroll` | base | true | n=1 |
| 6662 | `sidebar.section` | optional | true | n=2 |
| 6662 | `sidebar.heading` | optional | true | n=2 |
| 6662 | `sidebar.projectCreate` | optional | true | n=1 |
| 6662 | `sidebar.threadRow` | base | true | n=32 |
| 6662 | `sidebar.threadRoute` | base | true | n=32 |
| 6662 | `sidebar.threadActive` | base | true | n=32 |
| 6662 | `mode.switcher` | base | true | `nav` 作用域内排除 sidebar 后恰好 1 个触发器；id 为 `radix-_r_4_`（6321 是 `radix-_r_3_`，Radix 生成值本就不稳定，见 §5.5） |
| 6662 | `sidebar.headerActionSlot` | optional | true | 模式行恰好 1 个非模式按钮的元素子节点 |
| 6662 | `host.locale` | base | true | `en-GB` |
| 6662 | `titlebar.dragRegion` | base | true | 可见 `main` 作用域内检出 |
| 6662 | `theme.sidebarSurface` | base | true | `#f2f2f3`（`--vscode-sideBar-background`） |
| 6662 | `theme.mainSurface` | base | true | `#fafafa` |
| 6662 | `theme.dropdownSurface` | base | true | `rgb(251, 251, 251)` |
| 6662 | `theme.foreground` | base | true | `#383a42` |
| 6662 | `theme.secondaryText` | optional | true | `color-mix(in srgb, #383a42 65%, transparent)`。2026-08-16 由 `additive`(=base) 降为 `optional`，见 §8.2 |
| 6662 | `theme.border` | optional | true | `rgba(56, 58, 66, 0.078)`。同上降级 |
| 6662 | `theme.borderHeavy` | optional | true | `rgba(56, 58, 66, 0.117)`。同上降级 |
| 6662 | `theme.primary` | optional | true | `#526fff`（与 6321 相同）。同上降级 |
| 6662 | `theme.buttonForeground` | optional | true | `#fafafa`。**探针改读 `--vscode-button-foreground`**：原别名 `--color-token-button-foreground` 在 6662 已不存在，值不变，见 §8.1 |
| 6662 | `theme.linkForeground` | optional | true | `#526fff`。同上降级 |
| 6662 | `theme.inputBackground` | optional | true | `rgba(251, 251, 251, 0.96)`。**探针改读 `--vscode-input-background`**，原别名已不存在，值不变，见 §8.1 |
| 6662 | `theme.inputBorder` | optional | true | `rgba(56, 58, 66, 0.117)`。同上降级 |
| 6662 | `theme.hoverBackground` | optional | true | `rgba(56, 58, 66, 0.053)`。同上降级 |
| 6662 | `theme.fontSans` | base | true | `Maple Mono NF CN, -apple-system, …`（受用户字体设置影响，非稳定基线，见 §5.7） |
| 6662 | `theme.fontMono` | base | true | `Maple Mono NF CN, ui-monospace, …`（同上） |
| 6662 | `theme.radiusLarge` | optional | true | `calc(.625rem * 1.25)`。同 §8.2 降级；`applyRadiusTokens` 本就跳过读不到的半径 |
| 6662 | `theme.radiusMedium` | optional | true | `calc(.5rem * 1.25)`。同上降级 |
| 6662 | `theme.radiusSmall` | optional | true | `calc(.375rem * 1.25)`。同上降级 |

### 2.1 已使用但未被探测的锚点（升级盲区）

renderer agent 在运行时依赖、但 `SELECTORS` 完全没有覆盖的锚点。Codex 升级若改动
这些，probe 会全绿而功能已坏。

| 锚点 | 运行时用途 | 6119 | 6321 实测 |
|---|---|---|---|
| `[data-app-action-sidebar-thread-active="true"]` | `activeThreadId()`（`renderer-agent.ts:511`）读取"当前 thread"，是 ThreadBinding（#4）解析 Issue 的唯一入口 | 未记录 | **已补探针**：2026-08-10 加入 `SELECTORS`（`sidebar.threadActive`，`subtractive`）。属性存在（n=24），真实点击验证语义正确，见 §5.1 |
| `[data-app-action-sidebar-thread-selected]` | 当前**无任何运行时消费者**，故不单独建探针（探针只能断言存在，无法断言它与 `thread-active` 一致） | 未记录 | 属性存在（n=24），点击后与 `thread-active` 同步翻 `true` |
| `nav`（`sidebar.scroll` 的祖先） | 定位模式菜单触发器的作用域 | 未记录 | 存在（n=1） |
| `button[aria-haspopup="menu"][aria-expanded][data-state]` | 模式菜单触发器 | 未记录 | 文档内 n=9；`nav` 作用域内排除 sidebar 后 n=1 |
| `button.id`（模式触发器的 id） | `associatedModeMenu()` 靠 `aria-labelledby === button.id` 配对；缺 id 时 `ensureNativeCodexMode` 会强制 safe mode | 未记录 | **已补探针**：2026-08-10 起并入 `mode.switcher` 判定。实测 `radix-_r_3_` 非空。注意这是 Radix `useId` 生成的值，**不是稳定标识**，只能断言非空，不得断言具体值 |
| `[role="menu"]` / `[role="menuitem"]` | 注入 Weft 第三模式项、克隆原生条目样式 | 未记录 | 按需渲染：关闭时 n=0，展开后 menu=1 / menuitem=2。**已用真实点击验证**，renderer agent 的全部结构假设成立，见 §2.2。被动探针仍无法覆盖，归 #6 |
| `header` 内 `-webkit-app-region: drag` | workspace overlay 顶部偏移 | 未记录 | **已修作用域**：探针原为全文档（n=3），运行时只在 `main` 内查；2026-08-10 起探针改为"可见 `main` 作用域"，与运行时一致，实测 n=2 |

### 2.2 交互式验证（被动探针覆盖不到的部分）

模式菜单按需渲染，快照探针永远看不到它。2026-08-10 用 CDP 真实指针事件展开菜单后
逐条核对了 renderer agent 的结构假设——**全部成立**：

| renderer agent 的假设 | 代码位置 | 6321 实测 |
|---|---|---|
| 触发器 `id` 与菜单 `aria-labelledby` 配对 | `associatedModeMenu()`，`renderer-agent.ts:417` | 成立：菜单 `aria-labelledby = radix-_r_3_` = 触发器 id |
| 展开后存在 `[role="menu"]` | `renderer-agent.ts:414` 等 | 成立：menu=1，menuitem=2 |
| 原生条目可作克隆模板 | `renderer-agent.ts:461` | 成立：两项分别为 `ChatGPT` / `Codex` |
| 条目内叶子 `span` 承载文案 | `renderer-agent.ts:471` | 成立：每项 2 个叶子 span（标题 + 副标题），如 `["Codex", "Build, debug, and ship"]` |
| `/^Codex/i` 能命中 Codex 项 | `renderer-agent.ts:429`、`renderer-host.ts:223` | 成立 |
| 选中项带直接子 `svg`（原生勾选图标，需隐藏） | `renderer-agent.ts:491` | 成立且**只在选中项上**：Codex 项 svg=1、`hasDirectIconSvg=true`；ChatGPT 项 svg=0 |

按 `Escape` 后菜单干净收起（menu=0、`aria-expanded=false`），验证过程未残留状态。

> 这张表是**一次性人工验证记录**，不是回归基线——它依赖真实交互，CI 的被动探针复现
> 不了。要长期守住这些假设，需要 #6 用可交互的回归套件重放同一串操作。

### 2.3 两档挂载的真实注入验证（build 6321）

在**完整注入**的会话上实测两档（真 agent、真 iframe、真原生 sidebar）：

| 观测项 | Tier 2 `weft-mode` | Tier 1 `additive` |
|---|---|---|
| 原生子节点可见 | 0 / 2 | **2 / 2** |
| 可见会话行 | 0 | **24** |
| Weft root 高度 | 747px | **30px** |
| 入口按钮 | `none` | **`block`** |
| sidebar iframe | `block` | **`none`** |
| workspace overlay | `block` | `block` |

Tier 1 下原生 sidebar 完整保留，Weft 收缩为入口行，而 workspace overlay 仍可用——
即 §7.5 的「原生 UI 不动，仅注入 workspace 入口与视图」。

**方法上有两个坑，记下来免得重蹈：**

1. **不能用 stub 代替真 agent。** 早期验证只注入样式表加一个空 root，然后手改
   `data-weft-codex-tier`。那样测到的只是 CSS 选择器能否解析，证明不了注入行为。
2. **运行时改不了 tier。** 注入的 agent 会持续把该属性从 config 重新写回，手改立刻
   被覆盖；DOM 层面破坏锚点也撑不到 attach，因为 React 会重新渲染并恢复属性。
   要观测当时的 Tier 1，只能用当时的 `compatibilityTier: "additive"` 构造
   agent 源码并注入（agent 自带单实例守卫，会先 dispose 旧的）。2026-08-16
   起这条产品路径已删除：减法失败 = 不能进 Weft，不再有 additive 档可注入。

### 2.4 头部入口接管的真实注入验证（build 6321，2026-08-11）

在完整注入的 `weft-mode` 会话上实测（真 agent、真 iframe、真原生 header）：

| 观测项 | 实测 |
|---|---|
| `data-weft-codex-header-actions` | `native` |
| 被标记的原生 header action | 3 个（Search、View activity、New chat 块） |
| 其中仍可见的 | **0** |
| 注入的 Weft 入口 | 2 个，均可见，坐标 `x=189` / `x=219`——与原生两个按钮**原位重合** |
| 角标往返 | sidebar 数出 2 条 attention → `inbox.count` → agent 写角标；`aria-label` 实测 `Inbox, 2 needing attention` |
| 点击 → 面板 | sidebar iframe 收到 `{type:"weft:host-command", command:"search.open", version:1, origin:"app://-"}`，`.sidebar-panel` 出现 |
| `dispose()` 后 | 注入按钮 0、标记 0、`mode`/`headerActions` 属性均清空，原生 Search 与铃铛（含蓝点）恢复 |

**踩到一个坑，记下来免得重蹈**：第一次复验 `panel present: false`，一度以为是
消息链路有 bug。真实原因是 `ensureWeftd()` 会**复用任何在默认 URL 上健康的
daemon**——当时机器上另有一个 launcher 在跑，于是本次验证连上了它的 weftd，
拿到的是**另一个仓库的 UI bundle**，里面根本没有这次的改动。注入的 agent 是新的，
被注入的 UI 是旧的。

> 判据：验证注入行为时，**必须同时确认 daemon 服务的是哪一份 bundle**
> （`curl <weftd>/ | grep assets/`）。要跑私有实例就三个参数一起给：
> `--weftd-url=<空闲端口>`、`--weft-home=<私有目录>`、`--profile-dir=<私有目录>`。
> 只给 `--weft-home` 不给 `--weftd-url` 时，若默认 URL 上已有 daemon，
> `--weft-home` **完全不生效**（它只在 launcher 自己 spawn daemon 时才传下去），
> 数据会写进默认 home。

## 3. CSP / `frame-src` 行为矩阵

单独建表：这是 Chromium / CDP 层行为，不是某个 `data-app-action-*` selector，混进
§2 会让 `ok` 的语义变味。

| Build | `frame-src` 阻止 loopback iframe？ | 首次握手失败并触发 bypass 分支？ | bypass 需先启用再 reload？ | 退出恢复验证 | 依据 |
|---|---|---|---|---|---|
| 6119 | 是 | 是（走 `renderer-host.ts` 的 fallback 路径） | 是（对已提交文档不追溯生效） | 通过 | spec L335–338、L452–459 |
| 6321 | **是** | 是（同 6119，必然进入 fallback） | **是** | **通过** | 2026-08-10 直接测量，见下 |
| 6662 | **不适用**——Weft 已无 iframe | 是 | **是** | 通过 | 2026-08-16，见 §3.1 |

### 3.1 拦我们的已经不是 `frame-src`（6662，2026-08-16）

PR #72 之后 Weft 不再用 iframe。agent 现在只做两件事：`fetch()` 取 `/web/weft.css`
注入 shadow，`<script src>` 载 `/web/weft.js`。**因此 `frame-src` 这一列对 6662
已经没有意义**，上表保留历史行只为可审计。

6662 实测（专用 profile、`--safe-mode` 未注入，判定依据 `securitypolicyviolation`
事件而非 `load` / `error`）：

| 步骤 | 违规 | fetch | script | bundle |
|---|---|---|---|---|
| baseline（未 bypass） | `connect-src`（css）、`script-src-elem`（js），均 `enforce` | 失败 | error | 未载入 |
| `setBypassCSP(true)`，不 reload | 同上两条 | 失败 | error | 未载入 |
| `setBypassCSP(true)` + `reload` | **无** | **成功** | **成功** | **已载入** |

第二行说明「对已提交文档不追溯生效」在 6662 上依然成立，与 6119 / 6321 一致。

6662 完整策略（经 `<meta http-equiv>` 下发，这正是它不追溯的原因）：

```
default-src 'none';
script-src  'self' 'sha256-Z2/iFzh9VMlVkEOar1f/oSHWwQk3ve1qk/C2WdsC4Xk=' 'wasm-unsafe-eval';
connect-src 'self' https://ab.chatgpt.com https://api.mapbox.com https://cdn.openai.com
            https://events.mapbox.com https://learn.chatgpt.com
            wss://chatgpt.com wss://ws.chatgpt-staging.com wss://ws.chatgpt.com;
style-src   'self' 'unsafe-inline';
worker-src  'self' blob:;
frame-src   'self' blob: codex-sandbox://…;
```

`script-src` 与 `connect-src` 都是**显式**指令，都不含 `http://127.0.0.1`，文档
origin 为 `app://-`。**没有任何白名单条目可以让 loopback 通过**，所以
`Page.setBypassCSP` 不是权宜之计而是结构性必需——除非宿主主动加白名单。

> 踩坑记录：第一次测得「违规为零但 fetch 仍失败」，差点写成「bypass 只解了一半」。
> 真实原因是探针把路径写成了 `/weft.css`，而 weftd 挂在 `/web/` 下，返回 404。
> **CSP 拦截与 404 在 `fetch()` 里都表现为 `TypeError: Failed to fetch`**，无法靠
> 错误文本区分；必须看违规事件，并且独立确认目标 URL 真的可达（`curl` 一次）。

6321 的四列是用 `securitypolicyviolation` 事件直接测的，不是靠观察 launcher 是否进入
bypass 分支反推——后者只能间接推断，前者拿得到策略原文。四步实测结果：

| 步骤 | loopback iframe 被拦？ |
|---|---|
| baseline（未开 bypass） | **是**，`violatedDirective: frame-src`，`disposition: enforce` |
| `Page.setBypassCSP(true)`，**不** reload | **仍被拦** → 对已提交文档不追溯生效 |
| `Page.setBypassCSP(true)` + `Page.reload` | **不再被拦** → 必须先启用再 reload |
| `Page.setBypassCSP(false)` + `Page.reload` | **恢复拦截** → 退出恢复有效 |

策略经 `<meta http-equiv>` 下发（这正是它不追溯生效的原因：meta 在文档 commit 时解析），
6321 实测 `frame-src` 白名单为：

```
frame-src 'self' blob:
  codex-sandbox://*.web-sandbox.oaiusercontent.com
  codex-sandbox://web-sandbox.oaiusercontent.com
  https://*.web-sandbox.oaiusercontent.com
  https://web-sandbox.oaiusercontent.com;
```

其中**没有** `http://127.0.0.1`，文档 origin 为 `app://-`。结论：6321 与 6119 行为一致，
`Page.setBypassCSP` 仍然必要，spec L335 的断言可以从「当前 build 6119」改写为
「6119 与 6321 均如此」。

> 注意一个观测陷阱：被 CSP 拦截的 iframe 仍会触发 `load` 事件（停在 `about:blank`），
> 且跨域访问 `contentWindow.location` 无论放行与否都抛 `SecurityError`。因此判定必须
> 依据 `securitypolicyviolation` 事件，不能依据 `load`/`error` 或能否读到 location。

## 4. app-server 协议矩阵

按 `codex` CLI / app-server 版本分组，**不按 Desktop build 分组**。「对应 Desktop
build」列只作交叉参考，不表示绑定关系。

| `codex` 版本 | 探针 | 关键结论 | 对应 Desktop build | 日期 |
|---|---|---|---|---|
| 0.145.0 | `spike.py`（S5） | 活跃 turn 期间 `turn/start` 静默丢弃（返回成功但从不执行）；必须用 `turn/steer { threadId, expectedTurnId, input }` | 6119（同期机器） | 2026-08-08 |
| 0.145.0 | `probe_thread_source.py` | 无 `--session-source` 旗标；`thread/start { threadSource }` 原样落库到 `threads.thread_source`；`threads.source` 恒为 `"vscode"` | 6119（同期机器） | 2026-08-09 |
| 0.145.0 | `probe_turn_started.py` | 同进程 `turn/started` 必然到达（启动确认防线的前提） | 6119（同期机器） | 2026-08-09 |
| 0.145.0 | `probe_takeover.py` | turn 生命周期通知不跨 app-server 进程；第二进程接管时己方 watcher 收不到对方的 `turn/started` / `turn/completed` | 6119（同期机器） | 2026-08-09 |
| 0.145.0 | *（未重跑脚本）* | `codex --version` 实测仍为 `0.145.0`，**与 spike 时同一版本**，因此上面四行的协议结论按版本沿用，无需重跑消耗 turn 的脚本。改用零副作用的结构核对：`codex app-server generate-json-schema --out` 生成 39 个文件 / v2 共 532 个定义，逐一确认结论所依赖的形状仍在（见下） | 6321 | 2026-08-10 |
| 0.145.0 | *（未重跑脚本）* | 版本仍是 `0.145.0`，按同一判据沿用。结构核对复现：`ls` 39 条、v2 汇总 532 个定义，与 08-10 完全一致；下表五个形状逐一确认仍在 | **6662** | 2026-08-17 |

2026-08-10 的结构核对结果（v2 schema）：

| 定义 | 实测字段 | 对应结论 |
|---|---|---|
| `TurnSteerParams` | `threadId`, `clientUserMessageId`, `expectedTurnId`, `input` | 与 spec `turn/steer { threadId, expectedTurnId, input }` 一致，活跃期注入路径未变 |
| `ThreadStartParams` | 含 `threadSource`、`sessionStartSource` | `threadSource` 杠杆仍在 |
| `TurnStartParams` | 含 `outputSchema` | 结构化产出管线未变 |
| `ThreadResumeParams` | 含 `threadId` | 读完整历史仍走 `thread/resume` |
| `TurnStartedNotification` | `threadId`, `turn` | 启动确认防线的前提仍在 |

协议漂移基线（39 个 schema 文件的组合 sha256，供 CI diff 用）：

```
35f55ce111453c826d3ad43d940e8c299cd016060c1dc7ed2847cad63c06986a
```

> **这个哈希复现不出来，别拿它当回归信号。** 2026-08-16 用同一版本
> （`codex-cli 0.145.0`）重跑，两个结构数字精确复现——`ls` 39 条、v2 汇总文件
> 532 个定义——但三种拼接口径（顶层 `*.json` 按名、全部文件按路径、含目录）算出的
> 都不是它。当初没记录拼接方式，所以它只是一串无法验证的字符。
>
> 换成可复现的口径，命令写在这里：
>
> ```
> codex app-server generate-json-schema --out <dir>
> cd <dir> && find . -type f | sort | xargs cat | shasum -a 256
> ```
>
> 6662 / 0.145.0 实测：`daa0e817ed213fe83301696e474c666777878971282239ad0b1f872577efd8ec`
>
> 教训比哈希本身重要：**回归基线必须连同计算方式一起记录**，否则下一个人只能
> 推倒重来。

> 这条哈希才是协议轴的回归信号。**注意它只证明形状未变，不证明运行时行为未变**——
> 「活跃 turn 期间 `turn/start` 被静默丢弃」这类语义结论无法从 schema 读出，只有重跑
> `spike.py` 才能复验。版本号相同时沿用是合理的；版本号一旦变化，必须重跑四个脚本
> 而不是只比哈希。

> 重跑步骤：先 `codex --version` 记录版本号，再依次跑 `spike.py` →
> `probe_thread_source.py` → `probe_turn_started.py` → `probe_takeover.py`，结果追加
> 新行。**若 `codex` 版本与上次不同，即使 Desktop build 未变也要单独建行**——这正是
> 本表与 §1–§3 分轴的原因。

## 5. 已知增量与问题（6119 → 6321）

只记录**已确认**的差异与问题。

### 5.1 `thread-active` 语义：静态分析曾误判，运行时已澄清

`renderer-agent.ts:511` 用 `[data-app-action-sidebar-thread-active="true"]` 判定"用户
当前所在的 thread"，据此填充 `HostContextV1.threadId`——这是 #4 把原生 Thread 解析回
Issue 的唯一输入。

**运行时结论（2026-08-10，真实点击验证）：该判定在 6321 上是正确的。**

连续点击三个会话行，`thread-active` 与 `thread-selected` 每次都**同时**翻成 `true`，
且始终只落在同一行：

| 操作 | `active="true"` 的行 | `selected="true"` 的行 |
|---|---|---|
| 初始（无会话打开） | 无 | 无 |
| 点第 1 行 | `local:019fe0f9…` | `local:019fe0f9…` |
| 点第 3 行 | `local:019fbde4…` | `local:019fbde4…` |
| 点第 6 行 | `local:019f889b…` | `local:019f889b…` |

> **订正记录**：本节初稿依据 bundle 静态分析断言 `thread-active` 语义是"有活跃 turn
> 在跑"、`activeThreadId()` 已失效。该结论**是错的**。当时的依据是两点：二者是同一
> 组件的两个独立 prop（`String(active)` / `String(selected)`），且只有 `selected` 带
> `=true` 的选中态样式。这两点事实本身成立，但**推不出**语义分离——采集时恰好没有
> 任何会话打开，两者全为 `"false"`，静态证据被过度解读了。真实点击推翻了它。
>
> 教训：属性是否存在可以静态求证，属性的**语义**必须运行时验证。

仍然成立的问题是**探针覆盖**，而不是语义：`thread-active` 此前完全不在 `SELECTORS`
里，而它是 Issue 解析的唯一输入。2026-08-10 已补为 `sidebar.threadActive`
（`subtractive`）。

尚未验证的边界：二者既然是独立 prop，理论上可能解耦（例如多窗口、或路由切换过程中的
瞬时状态）。本次三次切换均未观察到解耦。探针只能断言属性存在、无法断言二者一致，
所以若 #4 要依赖"二者等价"这一点，应由 #6 的回归套件用真实点击守住。

### 5.2 `renderer.main` 是假阳性（6321 新增）

6321 的路由过渡会同时保留两个 `main`：

| index | display | visibility | pointerEvents | inert/aria-hidden | 可见面积 |
|---|---|---|---|---|---|
| 0 | flex | **hidden** | **none** | **是** | 950400 |
| 1 | flex | visible | auto | 否 | 735302 |

裸 `querySelector("main")` 命中 index 0——即那个**不可见、不可交互、inert** 的旧路由。
注意隐藏的那个**可见面积反而更大**，所以仅按面积排序的启发式同样会选错；必须先按
`inert` / `aria-hidden` / `hidden` / `visibility` / `pointerEvents` 过滤，再按面积取最大。

探针当前只判断 `Boolean(document.querySelector("main"))`，因此 `renderer.main` 报
`ok: true` 并不能保证 workspace 能挂到正确的路由上。

> 该修复由 `visibleMainRoute()` 实现（`VISIBLE_MAIN_HELPERS_SOURCE`，probe 与 renderer
> agent 共用同一套判定），随 `76bdbd3` 合入。本条记录用于说明为什么 6321 那次采集的
> `renderer.main: ok` 不可直接作为基线——当时探针还是裸 `querySelector("main")`。
>
> `titlebar.dragRegion` 的作用域修复已收敛到同一个 helper：探针现在只在
> `visibleMainRoute()` 返回的那个 route 内查找拖拽区域，与 `ensureWorkspaceRoot` 的
> 挂载目标完全一致，不再有第二份 inert/visibility 判定。

### 5.3 CDP target 选择在 6321 上不再唯一

6321 的应用会同时暴露两个 page target：

```
page  Codex  app://-/index.html
page  Codex  app://-/index.html?initialRoute=%2Favatar-overlay
```

`selectRendererTarget` 要求唯一命中，因此**不带 `--target-url` 的 probe 直接失败**：

```
weft-codex: Could not select one renderer target. Candidates: Codex (app://-/index.html?initialRoute=%2Favatar-overlay), Codex (app://-/index.html)
```

这是 fail-closed（不会误挂到 overlay），行为正确，但意味着任何自动化路径都必须显式
指定 target。overlay target 出现时机晚于主 target，所以启动早期可能只看到一个——
自动化不能依赖"只有一个 target"这个假设。

### 5.4 `sidebar.projectCreate` 探针命中真实属性（澄清）

6321 实测 `data-app-action-sidebar-project-create` 确实存在（n=1），
`data-app-action-sidebar-project-row` 也存在（n=4）。spec L455 当年只记录了后者，
造成"探针查的属性可能不存在"的疑虑——**该疑虑不成立**，探针命中的是真实属性。
因此把它升级为 `subtractive` 不会把所有安装钉死在 `additive`。

### 5.5 模式触发器的 id 由 Radix 生成

6321 实测模式切换触发器的 `id` 为 `radix-_r_3_`——Radix `useId()` 的产物，随渲染树
结构变化，**不是稳定标识**。`associatedModeMenu()` 只用它做 `aria-labelledby` 配对，
不依赖具体值，所以探针只断言"非空"。任何把该 id 硬编码或写进回归基线的做法都会在
下一次 Codex 结构调整时误报。

### 5.6 主色变更

`--color-token-primary` 与 `--color-token-text-link-foreground` 从 6119 的 `#0169cc`
变为 6321 的 `#526fff`。属正常主题演进，Host Context 已按 token 转发，无需代码改动；
记录在案以便回归时区分"token 消失"与"token 变值"。

### 5.7 字体 token 不是稳定基线

`theme.fontSans` / `theme.fontMono` 实测首位是 `Maple Mono NF CN`——用户本机字体设置
的产物，不同机器不同。这两项只应断言"非空"，不得断言具体值。

### 5.8 锚点分两类：结构依赖 vs 数据依赖

2026-08-10 的重分级把 `sidebar.threadRow` / `threadRoute` / `threadActive` 升为
`subtractive`，**这是错的**，当天即修正。

这三个锚点挂在会话行上，而会话行是**用户数据**：一个全新 profile 渲染出的 sidebar
完全健康，只是没有行可以承载这些属性。实测（移除全部 24 行模拟新 profile）：

| 场景 | 修正前 | 修正后 |
|---|---|---|
| 有会话 | `weft-mode` | `weft-mode` |
| 无会话（新 profile） | **`additive`** | `weft-mode` |
| 有会话但属性被改名 | `additive` | `additive` |

也就是说修正前**每一个新用户都会被永久钉在 Tier 1**。探针把"Codex 改了属性"和
"这个用户还没开始聊天"当成了同一件事。

修正方式是引入"不适用"：`threadRowCount === 0` 时这三条报 ok 并注明未验证，而不是
报失败。代价是明确的——如果 Codex 改的正是行属性本身，无会话的 profile 察觉不到；
但只要有一条会话就能察觉，且 §6 的演练直接覆盖改名场景。

同批修正的还有 `renderer-host.ts` 的 hydration 重试：它原本只在 `safe-mode` 时重试，
而 Codex 会先画出 sidebar 外壳再填会话列表（CSP reload 之后会再来一次），探针落在
这个窗口就会把整个会话钉在 `additive` 且不再重试。现改为在低于 `weft-mode` 时持续
重试到同一个 deadline。

**给后来者的判据：给一个锚点定 `requiredFor` 之前，先问它的缺失是否可能由用户数据
造成。会的话，它就不能无条件参与分级。**

### 5.9 侧边栏头部 action 槽位（2026-08-11，build 6321）

Weft 模式下原生的搜索与活动两个 icon 一直没被隐藏。原因是结构判断错了作用域：

```
div#HEADER  .relative z-10 flex shrink-0 flex-col gap-2 px-row-x …
├── div#MODEROW  .ms-2 flex items-center pe-1
│   ├── button                                    ← 模式切换器
│   └── div  .ms-auto flex items-center gap-1     ← action 槽位
│       ├── span > button   aria-label="Search"
│       └── span > button   aria-label="View activity, needs attention"
└── div  .flex flex-col gap-1                     ← markModeHeader 唯一标记到的
```

`markModeHeader()` 遍历的是 header 的其它子节点，而槽位在 **modeRow 内部**，
因此永远够不到。已改为同时标记槽位的子节点。

两个按钮**没有任何 `data-app-action-*` 属性**，唯一可区分的是 `aria-label`，
而它是 locale 文本（本次采集宿主为 `en-GB`）。**不得据此建锚点**——换个界面语言
就会失配。因此 `actionSlot()` 与探针都只用形状判定："modeRow 里除模式按钮之外
恰好一个元素子节点"，两边共用同一条判据，避免探针与运行时各说各话。

按钮的 class 串与模式切换器完全相同，所以克隆原生按钮即可继承宿主的尺寸、
hover、focus ring 与主题——与 `createWeftMenuItem` 克隆 `menuitem` 同一手法。
真机注入实测通过（隐藏原生两个 → 克隆注入两个 → 对齐/角标正确 → 还原干净）。

**`requiredFor` 定为 `optional`，这是刻意的**：槽位缺失只应让搜索与收件箱两个
入口改在 Weft sidebar 自己的头部渲染（Host Context 的 `headerActions:
"fallback"`），能力不丢，丢的只是位置。若定成 `subtractive`，一个纯装饰性的
结构变动就会把整个会话打到 Tier 1、连 Weft sidebar 一起丢掉——用一个功能性
回归换一个观感回归。判据同 §5.8：**先问这个锚点缺失的真实代价有多大。**

### 5.10 `⌘K` 属于宿主，且是菜单 accelerator 级别（2026-08-11，build 6321）

合成 `⌘K`（CDP `Input.dispatchKeyEvent`）**页面确实收到了**——capture 阶段监听器
录到 `{key:"k", meta:true, defaultPrevented:false}`——但**什么都没打开**。而点击
搜索按钮会开出 `[role="dialog"]`（"Command menu / Search commands and past chats"，
输入框 placeholder `Search chats`），证明检测器本身有效。

`⌘/` 调出的宿主快捷键表给出权威答案，摘录与本次改动相关的几条：

| 功能 | 绑定 |
|---|---|
| Open command menu | `⌘K`、`⇧⌘P` |
| Toggle activity view | `⌥⌘U` |
| Find | `⌘F` |
| Search Files… | `⌘P` |
| Switch to Chat / Work / Codex | `⌃1` / `⌃2` / `⌃3` |

即 `⌘K` 确实被宿主占用，但绑定在 **Electron 菜单 accelerator 层**，CDP 注入的键
事件绕过了 OS 菜单层，所以合成事件打不开它。

**推论方向与直觉相反**：不是"renderer 能拦截但需要拦截"，而是 renderer 层大概率
**根本收不到真实 `⌘K`，也就无从拦截**。合成事件无法证明真实按键是否会漏到
iframe，所以不赌——Weft 搜索改用 `/` 聚焦，不建立在 `⌘K` 上。`kanban-view.tsx`
原先展示的 `⌘K` 提示在 Desktop 下是误导，已一并改掉。收件箱同理不抢快捷键
（活动视图另有 `⌥⌘U`）。

> 方法论：这条与 §5.1 是同一个坑的两面。§5.1 是**静态证据过度解读**，这条是
> **合成事件的 null 结果**——两者都不能当结论用。`⌘K` 无反应的真正原因要靠
> 宿主自己的快捷键表交叉验证才能确定，而不是从"按了没反应"直接推出"没绑定"。

## 6. 升级回归演练

单测能断言分级表与本文件一致，但断言不了"我们分级的锚点在发行版里真的存在"，
也断言不了"少一个锚点时真的会降级"。这两件事由 `scripts/upgrade-drill.mjs` 覆盖：
它在活的 renderer 上改掉锚点属性名（模拟一次 Codex 升级），走**真实探针管线**
（`buildProbeExpression` + `reportFromSnapshot`）读出 tier，然后还原。

```
node launcher/dist/cli.js start --safe-mode --debug-port=9227 --profile-dir=/tmp/weft-codex-drill
node scripts/upgrade-drill.mjs 9227
```

**不要对着日常使用的 Codex 跑**——它会改 DOM 属性。脚本每次都还原，并在结尾复验
tier 已回到基线；但中途崩溃会留下被改过的 renderer，重新加载窗口即可恢复。

### build 6321 实测（2026-08-10）

| 步骤 | tier | 失败探针 |
|---|---|---|
| 基线 | `weft-mode` | 无 |
| 改名 `data-app-action-sidebar-thread-id`（22 个节点） | **`additive`** | `sidebar.threadRoute(subtractive)` |
| 还原 | `weft-mode` | 无 |
| 改名 `data-app-action-sidebar-scroll`（1 个节点） | **`safe-mode`** | `sidebar.scroll(base)`、`mode.switcher(subtractive)` |
| 还原 | `weft-mode` | 无 |

两点值得记下：

- 这条演练同时证明了 2026-08-10 的重分级是有效的。`sidebar.threadRoute` 原为
  `optional`，同样的改名在重分级之前会让 tier **纹丝不动**——正是"全绿着坏掉"。
- base 锚点失效会**级联**让 `mode.switcher` 一起失败：模式触发器的发现作用域是
  `sidebar.closest("nav")`（`renderer-agent.ts`），sidebar 找不到时触发器也就找不到。
  因此 `mode.switcher` 的失败不一定代表菜单本身有问题，排障时应先看 `sidebar.scroll`。

### build 6662 实测（2026-08-16）

演练本身也随两档模型翻修过：原先第一条用例期待 `additive`，而该档 2026-08-16 起
已不存在，脚本因此一直是坏的——6662 的回归没被它拦下，这是原因之一。现在改为覆盖
四个分级方向，并新增 token 用例（置空手法见脚本注释：inline 空白值的 computed 值
为空串，`removeProperty` 原样还原）。

| 步骤 | tier | 失败探针 |
|---|---|---|
| 基线 | `weft-mode` | 无 |
| 改名 `data-app-action-sidebar-scroll`（1 个节点） | **`safe-mode`** | `sidebar.scroll(base)`、`mode.switcher(base)`、`sidebar.headerActionSlot(optional)` |
| 改名 `data-app-action-sidebar-thread-id`（32 个节点） | **`safe-mode`** | `sidebar.threadRoute(base)` |
| 改名 `data-app-action-sidebar-project-create`（1 个节点） | `weft-mode` | `sidebar.projectCreate(optional)` |
| 置空 `--color-token-main-surface-primary` | **`safe-mode`** | `theme.mainSurface(base)` |
| 置空 `--vscode-button-foreground` | `weft-mode` | `theme.buttonForeground(optional)` |
| 还原 | `weft-mode` | 无 |

最后两行是 §8.2 那次降级的真机证据：核心表面缺失照样挡住，配色 token 缺失只报出
失败、不再拦人。上一段记录的级联现象在 6662 上复现，只是 `mode.switcher` 现在报
`base`（6321 时记为 `subtractive`）。

## 7. 维护规则

- 只追加，不改写历史行。需订正历史行时另起一行并注明「订正 YYYY-MM-DD，原行见上」，
  保留可审计轨迹。
- `ok` 只有 `true` / `false` / `未记录` 三种取值；**禁止把未留证的锚点填成 `true`**。
- 采集必须在 `--safe-mode`、专用 profile、未注入的条件下进行，否则测到的是 Weft 自己
  改过的 DOM。若某项只能在注入路径下观测（如 §3 的 CSP），单独标注采集条件。
- Anchor id 与 `launcher/src/probes.ts` 完全一致。**这条由测试强制**：
  `probes.test.ts` 的「the compatibility matrix documents every probe of the newest
  build」会解析 §2 中 build 号最大的那组行，与 `reportFromSnapshot` 的实际输出对账；
  新增探针未补文档行、或改了 `requiredFor` 未同步，都会让测试失败。因此本文件不是
  可选文档，而是构建的一部分。
- spec `docs/specs/2026-08-08-codex-desktop-migration-design.md` 中的 build / 协议版本
  断言，在重大版本变更后应指回本文件，而不是在 spec 正文里继续累积日期戳。

### 7.1 采集样式数值的四条纪律

`DESIGN.md` 的每个数值都出自这里，重测时按这四条来——它们分别对应 §5.1、§5.10、
§2.4 记录过的一次误判。

1. **测伪类状态用 `CSS.forcePseudoState`，不要用 `.focus()`。** 程序化 focus 不匹配
   `:focus-visible`，据此得出的「没有 focus 环」是假阴性。
2. **先确认 daemon 服务的是哪份 bundle。** `ensureWeftd()` 会复用默认 URL 上**任何**
   健康的 daemon。跑私有实例三个参数一起给：
   `--weftd-url=<空闲端口>`、`--weft-home=<私有目录>`、`--profile-dir=<私有目录>`；
   只给 `--weft-home` 而默认 URL 上已有 daemon 时它完全不生效。采集前
   `curl <weftd>/ | grep assets/` 对一下 hash。
3. **无宿主路径这样测**：剥掉 sidebar iframe `documentElement` 上宿主写入的全部 inline
   自定义属性，同一张样式表即落到 `--fb-*`，等价于标准的无宿主状态，且保证是同一份
   构建。
4. **null 结果不是结论。** 合成事件没反应可能是事件没送达（`⌘K` 被 OS 菜单层拦下
   即此例）。先用已知为真的目标验证探针本身有效，再解读 null。

## 8. 已知增量与问题（6321 → 6662）

本节是本矩阵**第一次真的抓到回归**——而且是在用户机器上先坏、事后才补记录的，
不是 CI 拦下来的。宿主于 2026-08-15 20:24 自更新到 6662，次日第一次真机启动即
落进 safe mode。

### 8.1 别名层收缩：两个 `--color-token-*` 消失

`--color-token-button-foreground` 与 `--color-token-input-background` 在 6662
上读不出值。这不是主题差异：同一次采集里其余 16 个 token 的值与 6321 行**逐字节
相同**，字体、圆角、主色全部未变。

三处证据一致：

| 观测 | 结果 |
|---|---|
| `app.asar` 内出现次数 | 两个名字均为 **0**（`--color-token-primary` 23 次、`--color-token-input-border` 2 次） |
| 运行时 `getComputedStyle(document.documentElement)` | 两项均为空串 |
| 底层 `--vscode-*` | `--vscode-button-foreground` = `#fafafa`，`--vscode-input-background` = `rgba(251, 251, 251, 0.96)` |

Codex 的 `--color-token-*` 本就是架在 `--vscode-*` 上的一层别名，包内可见其定义
形如 `--color-token-input-border: var(--vscode-input-border, transparent)`。6662
只是把这两条别名从该块里删掉了，底层变量与值都还在——所以探针改读 `--vscode-*`
既恢复判定，又不改变任何观感。`theme.sidebarSurface` 从一开始就是这么取的，本次
只是让另外两项跟上同一做法。

同批修的还有 renderer agent 收件箱角标：它是全代码库**唯一**一处不带兜底的
`var(--color-token-button-foreground)`，6662 上角标文字会掉回继承色、压在主色底
上。`ui/src/index.css` 的 5 处消费点都带 `--fb-*` 兜底，因此只损失宿主保真度
（`--fb-on-accent: #ffffff` vs 宿主 `#fafafa`），不影响可用性。

**收缩的不止这两个。** 2026-08-16 把 UI 消费的 15 个 `--color-token-*` 与 6662
实测清单逐个对账，共 **6 个**已不存在——除探针那两个外，还有三条语义色和占位符色，
它们一直在静默走 `--fb-*`，也就是说 Weft 的成败/警告/占位符颜色早已与宿主脱钩：

| UI 变量 | 失效的别名 | 6662 仍在的底层 |
|---|---|---|
| `--on-accent` | `--color-token-button-foreground` | `--vscode-button-foreground` |
| 字段底（4 处） | `--color-token-input-background` | `--vscode-input-background` |
| `--danger` | `--color-token-charts-red` | `--vscode-charts-red` |
| `--ok` | `--color-token-charts-green` | `--vscode-charts-green` |
| `--warn` | `--color-token-charts-yellow` | `--vscode-charts-yellow` |
| `--placeholder` | `--color-token-input-placeholder-foreground` | `--vscode-input-placeholderForeground` |

`--color-token-charts-blue` 反而还在，说明这不是整族下线而是逐个收缩，**下次升级
仍需逐名对账**，不能按族推断。

消费点改为三段兜底链：别名（旧构建）→ `--vscode-*`（6662）→ `--fb-*`（无宿主）。
链尾不变，所以这个改动只可能提高保真度、不可能比原状更差。

采集说明：`button-foreground` / `input-background` 在明暗两套主题下都实测过；
三条 charts 与 placeholder 只在暗色主题实测。它们是 VS Code 标准色注册表里的名字，
但本文件不据此推断——**未实测就是未记录**，明色一栏留待下次采集。

### 8.2 token 分级收回 spec 的核心档

真正让两个配色别名足以杀死整个产品的，是 `tokenProbe()` 把 18 个 token **一律**
写死成 `requiredFor: "base"`。这比 spec 严：08-16 spec §8.3 写的是「缺**核心**表面 /
前景 / 字体则探针失败」，§8.4 表格写的是「主题**核心** token」。

现在 `base` 只留 spec 点名的六项：`theme.sidebarSurface` / `mainSurface` /
`dropdownSurface`（表面）、`theme.foreground`（前景）、`theme.fontSans` /
`fontMono`（字体）。其余 12 项降为 `optional`——照常探测、照常在 `detail` 里报出
缺失，只是不再一票否决。

判据与 §5.8 是同一条，只是换了个轴：**定 `requiredFor` 之前先问缺失的真实代价。**
`sidebar.scroll` 没了就没地方挂 Weft 侧栏，是功能损失；`--color-token-primary`
没了只是主色掉回 `--fb-accent`，是保真度损失。用 safe mode 惩罚后者，等于拿一次
功能性回归去换一次观感回归——正是 §5.9 定 `headerActionSlot` 为 `optional` 时
拒绝过的那笔交易。

历史行的 `additive` 与当时的 `base` 同义（见 §2 表头注），本次降级只体现在 6662
行；6119 / 6321 行按采集当时的分级保留。

### 8.3 右侧 / 底部面板：新的运行时依赖，且探针够不着

workspace 让位几何（`nativePanelSize()`）开始依赖这两个锚点：

| 锚点 | 运行时用途 | 6662 实测 |
|---|---|---|
| `[data-app-shell-focus-area="right-panel"]` | workspace root 的 `right` 内缩量 | `<aside>`，可见 `main` 的后代 |
| `[data-app-shell-focus-area="bottom-panel"]` | workspace root 的 `bottom` 内缩量 | `<div>`，同上 |

**它们按需渲染，被动探针永远看不到**——和 §2.2 的模式菜单同类。Codex 没打开过侧栏
时整个 `data-app-shell-focus-area` 属性在文档里一次都不出现；第一次探到空数组不等于
锚点消失。因此不给它们建 `SELECTORS` 探针，归 §2.1 的升级盲区。

它们一旦创建就**留在 DOM 里**，关闭时塌成 inline `width: 0px` / `height: 0px` 并
`opacity: 0`。这让「量盒子」成为完整判据：关着读 0，没创建过也读 0，两种情况下 Weft
本来就不欠它空间。缺失即 0 是 fail-open 的正确方向，不需要额外的开合信号。

面板带过渡动画，所以必须把面板本身喂给 `ResizeObserver`：只在 inline style 翻转那一刻
量一次，会把 workspace 冻在动画中间尺寸。顺带也就免费拿到了用户拖分隔条的跟随。

6662 还新增了 `data-app-shell-tabs` / `tab-strip-controller` / `tab-controller` /
`tab-panel-controller` / `tab-close-button` / `tab-separator`：右侧面板已经是**带原生
tab 条的容器**（Review ⌃⇧G / Terminal ⌃` / Browser ⌘T / Files ⌘P），不再是一块
「谁占谁的」单槽。容器侧的结论与落地顺序见
`docs/specs/2026-08-16-host-container.md` §5。
