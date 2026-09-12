---
version: "alpha"
name: Weft in Codex
description: 宿主即真相。Weft 不建立独立于 Codex 的视觉语言，取值一律来自宿主 token。
omitted:
  - colors
rounded:
  sm: 7.5px
  md: 10px
  lg: 12.5px
  xl: 15px
  full: 999px
spacing:
  xs: 4px
  sm: 8px
  md: 12px
  lg: 16px
  xl: 20px
typography:
  root:
    fontSize: 16px
    lineHeight: 1.45
  heading:
    fontSize: 16px
    lineHeight: 1.5
  body:
    fontSize: 14px
    lineHeight: 1.45
  label:
    fontSize: 13px
  meta:
    fontSize: 12px
    lineHeight: 1.35
components:
  input:
    rounded: "{rounded.md}"
    height: 32px
    padding: 6px 9px
    typography: "{typography.body}"
  textarea:
    rounded: "{rounded.lg}"
    padding: 6px 9px
    typography: "{typography.body}"
  select-trigger:
    rounded: "{rounded.md}"
    height: 32px
    padding: 0 8px
    typography: "{typography.body}"
  select-content:
    rounded: "{rounded.xl}"
    padding: "{spacing.xs}"
  select-item:
    rounded: "{rounded.lg}"
    padding: 5px 8px
    typography: "{typography.body}"
  header-entry:
    size: 26px
    rounded: "{rounded.md}"
---

## Overview

Weft 嵌在 Codex Desktop 内运行，不建立独立于宿主的视觉语言。

> **宿主是真相，不是参考。** 控件在宿主里有对应物，就照抄它的构造；没有对应物，
> 才自行决定。

**偏离宿主的三步**：找最近的宿主对应物并继承其构造 → 只在确有必要处偏离且幅度
最小 → 理由写进代码注释与「Do's and Don'ts」。宿主对同一件事有两种行为时跟通则、
不跟例外，那不算偏离。

颜色、字体族、圆角尺度由宿主在运行时提供，本文记录的是**角色与构造**。参考值取自
Codex Desktop build 6321（2026-08-11），随宿主版本与主题变化，重测流程见
`docs/compat/codex-builds.md`。

## Colors

Weft 没有自己的色板（front matter 中 `colors` 为 `omitted`）。语义角色映射到宿主
token，由 `ui/src/index.css` 统一声明；**组件中不出现字面颜色值**。

| 角色 | 变量 | 宿主 token | 用途 |
|---|---|---|---|
| 前景 | `--text` | `--color-token-foreground` | 正文、图标 |
| 次级前景 | `--dim` | `--color-token-text-secondary` | 元信息、占位说明 |
| 三级前景 | `--dim-2` | `--color-token-text-tertiary` | 分区标题——宿主用它把标题压到行文本之下 |
| 描边 | `--border` | `--color-token-border` | 浮层轮廓、分隔线、焦点环 |
| 强描边 | `--border-strong` | `--color-token-border-heavy` | 字段轮廓 |
| 悬停 | `--hover` | `--color-token-list-hover-background` | 行与触发器的悬停底 |
| 页面 | `--panel` | `--color-token-main-surface-primary` | 工作区、看板卡等贴页表面 |
| 浮层 | `--panel-2` | `--color-token-dropdown-background` | 弹窗、下拉、toast、字段底 |
| 主色 | `--accent` | `--color-token-primary` | 选中标记、角标 |
| 焦点 | `--focus` | `--color-token-border` | 焦点环 |

**焦点色即描边色**，不是独立角色——宿主在自身控件上声明
`focus-visible:outline-2 outline-offset-2 outline-token-border`。它约 8% alpha，
对比度低于常见无障碍基线，是对齐宿主的既定代价；变更属于产品决策，先改 PRODUCT.md。

**强调用透明度表达，不换色相。** 悬停加前景 alpha 底色，字段悬停提升轮廓 alpha。
不引入主色悬停。

**页面与浮层不是同一层。** `--panel` 是贴在工作区上的表面；弹窗、下拉、toast
必须用 `--panel-2`。暗色宿主的 `--color-token-main-surface-primary` 可以是近黑
甚至纯黑，拿它做模态底会把对话框画成一块黑洞。

**无宿主环境沿用同一套角色。** `--fb-*` 复刻宿主约定而非另立一套：焦点在宿主内是
描边色，降级环境即取降级自身的描边色。

## Typography

字体族由宿主提供（`--font` / `--mono`），**不指定字体族**——用户自定义字体会改变
实测值，字体 token 只断言非空。

根字号 16px，与宿主一致。

此处曾写「13px（宿主 16px，Weft 表面信息密度更高）」。那条自定的密度差是走样的
源头：整张字号表都比宿主小一到两档，于是每次照宿主实测值落地都不敢直接用，只能
按比例换算，换算又生出新的偏差。**2026-08-16 起不再自定密度**——尺寸直接取宿主
实测值。

宿主整个界面只有四个字号，层级几乎全部由**字重**承担（445 / 500 / 600），
而不是由字号承担：

| 角色 | 字号 | 行高 | 宿主实测依据 | 用途 |
|---|---|---|---|---|
| `heading` | 16px | 1.5 | turn 标签 `You said:` 16/445/24 | 视图标题、详情与弹窗标题 |
| `body` | 14px | 1.45 | 侧栏会话行 14/445/20；消息正文 14/445/22 | 正文、行标题、控件文本、分区标题 |
| `label` | 13px | — | 导航项 `Pull requests`、模型名、`Bash` 皆 13/445/18 | 计数、字段标签、次级 chrome |
| `meta` | 12px | 1.35 | 时间戳 `Saturday 20:10` 12/445/16 | 次要说明、类型徽标 |

新增文本从上表取值，**不要新增第五档**。原先的 `title` / `subtitle` 并入
`heading` 与 `body`——宿主没有这两档，它靠字重区分。

**宿主全界面零处全大写文本，也零处字距调整。** 采集范围覆盖侧栏、主区会话、
composer 与模型选择器，`text-transform` 与 `letter-spacing` 全部是 `none` /
`normal`。原先的 `micro-caps`（9.5px 全大写 + 0.03em）因此整档删除，类型徽标
按 `meta` 走正常大小写；`entries.kind.*` 本来就写作 `Issue` / `Task`，去掉
CSS 大写即按原文渲染。

**分区标题不缩小、不大写。** 宿主把 Projects / Recents 设成和会话行同一字号，
只用字重与颜色拉开——500 对行的 445，`--dim-2` 对行的前景。Weft 照此办理。

## Layout

间距基于 4px 栅格。

| 级别 | 值 | 用途 |
|---|---|---|
| `xs` | 4px | 图标与文本间距、浮层内边距 |
| `sm` | 8px | 控件内水平间距、行间距 |
| `md` | 12px | 相邻控件组 |
| `lg` | 16px | 区块间距 |
| `xl` | 20px | 主区域留白 |

控件基准高度 **32px**，与宿主一致。

## Elevation & Depth

**边界由 ring 与投影表达，不使用 `border`。** 宿主全程如此：其 composer 为
`border: 0` 配 `0 0 0 0.5px` 描边环加柔和投影，浮层为同一构造的更低 alpha。1px 实线
边在其旁明显更重。

ring 的**颜色**区分可交互与否，**投影的深度**区分贴在页面上还是浮在其上：

| 层级 | 构造 | 用于 |
|---|---|---|
| 内联表面 | `0 0 0 0.5px var(--border)` | 看板列、任务列表、仓库块、详情头、代码区 |
| 抬升表面 | 内联 + `0 3px 7.5px rgb(0 0 0/.04)` | 看板卡片 |
| 字段 | `0 0 0 0.5px var(--border-strong)` + `0 3px 7.5px rgb(0 0 0/.04)` | input、textarea |
| 字段·悬停 | ring 提至 `color-mix(in srgb, var(--text) 26%, transparent)` | input、textarea |
| 字段·错误 | `0 0 0 1px var(--danger)` + 同上投影 | `aria-invalid` |
| 分栏边缘 | `-8px 0 16px -8px rgb(0 0 0/.18)` | stage 内详情分栏的左缘。**不配描边线** |
| 浮层 | 抬升 + `0 12px 32px rgb(0 0 0/.12)` | select-content、toast |
| 模态 | `0 0 0 0.5px var(--border)` + `0 24px 64px rgb(0 0 0/.18)` | 宿主级弹窗 |

**分栏边界只用投影，不画线。** 宿主的右侧面板实测就是这样：手柄里那条 1px 内线
常态透明，边界完全由那道向左的方向性投影表达。给分栏补一条 hairline 会比宿主重。

**语义状态改 ring 的颜色，不要退回 `border-color`。** toast 的成败、告警块的边都
走同一条 ring，换的只是颜色。

**但不要给容器描彩色边来表达状态。** 宿主从不为此着色容器；一圈环绕整卡的彩色轮廓
比它所报告的事情还响。状态由内容承载——文字加颜色，容器保持普通边。这同时满足
「状态不能只依赖颜色」：文字本身已经说清楚了。

chip / tag / 徽标用 **inset ring**：`box-shadow: inset 0 0 0 0.5px …`。它们尺寸小、
常处在紧凑行里，inset 落在原 `border` 的位置，不会被父级 `overflow` 裁掉，也不改变
外框尺寸。

`border` 只保留 `1px solid transparent` 一种用法：占位，使 ring 与 hover 不引起尺寸
跳动。

### 焦点环

按钮、链接、下拉触发器、`summary` 没有光标，焦点用一圈 outline 表达：

```css
outline: 2px solid var(--focus);
outline-offset: 2px;
```

由 `index.css` 单条规则统管，**不叠加 ring 或 box-shadow**。

**文本字段不画 outline。** 光标就是焦点信号。字段已有 hairline ring 表达可交互，
再套一圈 outline 只是给正在输入的盒子描边。这是通则，不是 command menu 或看板
搜索的例外。Select 触发器没有光标，仍走焦点环。

## Shapes

**圆角随元素高度递增**，这是宿主自身的规律：图标触发器（26px 高）10px · 菜单项与
行按钮 12.5px · 模式按钮与浮层 15px · composer 25px。

| 级别 | 值 | 用于 |
|---|---|---|
| `sm` | 7.5px | 树行、chip、徽标等低于宿主最小控件高度的元件 |
| `md` | 10px | 单行控件、图标按钮、结果行 |
| `lg` | 12.5px | 列表行、菜单项、多行控件 |
| `xl` | 15px | 浮层、模态 |
| `full` | 999px | 胶囊：状态 chip、计数徽标 |

`sm` 之下宿主没有对应物——它承接的是比 26px 更矮的行，取值继续沿这条斜坡下延，
而非套用一个在该高度会被钳成胶囊的更大值。

`xl` 无对应宿主 token（`--radius-lg` 为 12.5px），为实测字面值；发现对应 token 后
换回引用。

**圆角 token 必须以 px 转发。** 宿主以 rem 声明，而 rem 按消费方根字号解析
（宿主 16px、Weft 13px），直接转发字符串会使圆角整体缩小 19%。

## Components

### Input

单行文本字段。`{rounded.md}` · 32px · `6px 9px` · `{typography.body}`。

| 状态 | 表现 |
|---|---|
| 默认 | 表面底 + 字段级 ring |
| 悬停 | ring 提至 26% 前景 alpha |
| 焦点 | 无 outline，ring 不变；光标即焦点信号 |
| 错误 | ring 转 `--danger` |
| 禁用 | `opacity: .5`，`cursor: not-allowed` |

### Textarea

多行文本字段。同 Input，圆角取 `{rounded.lg}`，最小高度 96px，仅纵向可缩放。

### Select

**下拉一律为触发器加浮层，不使用原生 `<select>`。** 宿主全应用 0 个 `<select>`、
0 个 `aria-haspopup="listbox"`、14 个 `aria-haspopup="menu"`。实现基于 Radix Select，
以获得方向键、Home/End、首字母跳转与 Escape。

**Trigger** — `{rounded.md}` · 32px · `0 8px` · 透明底 · `1px solid transparent`。

| 状态 | 表现 |
|---|---|
| 默认 | 全透明，仅文本与 chevron |
| 悬停 / 展开 | `--hover` 底色 |
| 焦点 | 焦点环 |
| 占位 | 文本转 `--placeholder` |
| 禁用 | `opacity: .5`，`cursor: not-allowed` |

**Content** — `{rounded.xl}` · `{spacing.xs}` 内边距 · 表面底 · 浮层级 elevation ·
最小宽度对齐触发器。

**Item** — `{rounded.lg}` · `5px 8px`。

| 状态 | 表现 |
|---|---|
| 默认 | 无底色 |
| 高亮 | `--hover` 底色 |
| 选中 | 行尾 `--accent` 勾选标记 |
| 禁用 | `opacity: .5` |

### 列表行的尾部槽位

宿主的会话行把次级控件（Pin / Archive）放在行尾，**静置 `opacity: 0`、hover 转 1**，
空间始终预留所以不跳动（build 6321 实测）。行本身是一个扁平的点击目标，没有展开
控件。

Weft 的 issue 列表跟这条通则：一行一个点击目标，打开 Primary Lead，**不放展开箭头**。
会话树只在当前打开的 issue 下面出现；收起控件在那一块，不在每条列表行上。

### Header entry

注入宿主侧边栏头部的 Weft 入口，26px 方形，克隆宿主原生按钮以继承其尺寸、悬停、
焦点与主题。角标为 `--accent` 圆点，14px，右上角外溢 1px。

### 快捷键

宿主已占用下列绑定，且绑定在 Electron 菜单层，渲染进程无法拦截。**新增快捷键前先
以 `⌘/` 调出宿主快捷键表核对。**

| 功能 | 绑定 |
|---|---|
| Open command menu | `⌘K`、`⇧⌘P` |
| Toggle activity view | `⌥⌘U` |
| Find | `⌘F` |
| Search Files… | `⌘P` |
| Switch to Chat / Work / Codex | `⌃1` / `⌃2` / `⌃3` |

Weft 搜索使用 `/`，无修饰键，需守卫输入类元素。

## Do's and Don'ts

**Do** 以宿主 token 表达颜色与尺度。
**Don't** 在组件中写入字面颜色值或自定尺度。

**Do** 用 ring 与投影表达边界。
**Don't** 用 `border` 画字段与浮层的边。

**Do** 让按钮与触发器的焦点只有一层 outline。
**Don't** 给文本字段叠加 outline——光标已经说明焦点在哪。
**Don't** 叠加组件自带的 ring 工具类——它们走 box-shadow，不会被元素规则覆盖。

**Do** 把视觉属性写在 `ui/src/index.css`，组件只保留结构与行为。
**Don't** 在组件上堆 Tailwind 视觉类——`index.css` 的裸元素规则未分层，恒定胜过
分层的 utilities，这些类不会生效。

**Do** 以结构定位宿主元素。
**Don't** 以 `aria-label` 或文案定位——它随宿主界面语言变化。

**Do** 保持无宿主环境与宿主内一致。
**Don't** 为降级路径单独设计。

**Do** 修改样式前用真机计算值确认目标规则生效。
**Don't** 依据阅读推断——无人引用的类名与被整段覆盖的声明都能长期存活。
