# Weft · LOOM 落地推进计划

> ⚠️ **部分内容已过时(superseded,2026-06-10)**:本稿写于 PTY/xterm 内嵌终端形态时期。该形态已整体移除(commit `2d26038`),全部会话改走 headless chat 引擎(`src-tauri/src/lead_chat/`)。文中涉及 PTY/xterm/合帧/`.term` 容器的条目仅作历史参考,以 `ARCHITECTURE.md` 现行版为准。

从「设计提案 + 落地包」到**完整落地**的施工图。配套:[HANDOFF.md](HANDOFF.md)(token/术语/组件/规范)、原型 `designs/weft-redesign/`、drop-in [m-a-tokens.css](m-a-tokens.css)。

> **交付边界**:当前止于 **Task → PR**(每仓干净 PR + 仓库自带 CI 触发)。北极星 **Task → 上线**(merge → staging → production)是后续维度,编排现有 CD,不在本计划主体。

---

## 0. 前置(必须先做)

- **工作树清干净**:当前 `src/board/ThreadBoard.tsx` 有未提交改动。先 commit / stash,再从干净 `main` 起 `loom-reskin` 分支。换肤改动不应和你在写的功能混在一个 working tree 里。
- **基线截图**:落地前对现有 app 关键界面截图存档,便于换肤前后对比 + 回归。

---

## 三条轨道(可并行)

- **轨 A · 前端落地**(M-A→M-F):纯前端,大部分零/低风险,按里程碑逐 PR。
- **轨 B · 后端补点**:设计已就绪、缺数据源的几项;前端某些里程碑要等它。
- **轨 C · 横切**:a11y / 性能 / i18n,贯穿全程,每个 PR 顺手带。

---

## 轨 A · 前端落地(逐 PR,低→高风险)

### M-A · 换肤(✅ drop-in 已就绪)
- **做**:`m-a-tokens.css` 两个 `:root` 块替换进 `src/index.css`;加 Geist 字体(`@fontsource` import + `--font-sans`);`.term`(xterm 容器)固定深色;加 `.issue-num` 不显示编号(见 M-B,实为删除)、weave 线类、status pulse。
- **文件**:`src/index.css`(+ 可能 `tailwind` 无需动,`@theme` 不变)。
- **验收**:整库换成 LOOM 配色;AA 对比度全过;明/亮切换正常;嵌入终端两主题都深色;无组件改 className。
- **风险**:🟢 极低(纯 CSS 变量值)。**当天可见。**
- **依赖**:无。

### M-B · 术语 + 去编号
- **做**:`src/i18n/{en,zh}.ts` 全量:thread→issue、direction→子任务/sub-task、控制塔→控制台/Console、需要你→待你处理/Needs you、总线→Agent 协作、物化→建副本、profile→盘点、provenance→溯源。**不显示 issue 编号**(标题识别;PR 号照常)。
- **文件**:`i18n/en.ts` `i18n/zh.ts` + 用到旧词的组件文本 key。
- **验收**:中/英全量切换无漏译;内部状态枚举仍英文;无 `#编号` 出现在 issue 标识处。
- **风险**:🟢 低(文案)。
- **依赖**:无(可与 M-A 同 PR 或紧随)。

### M-C · IA 重心(最大的一刀)
- **做**:① 默认界面 = **控制台**(把 `LeadTab` 提升为 home,对话流出结构化卡:scope/派发/升级);看板退为同伴视图。② **待你处理 dock** 常驻、仅工作界面(控制台/Scope/看板/会话)、置顶,空态=「自动流转中」。③ 导航栏统一(顶栏全局 + dock 仅工作界面 + 页头仅有操作时;纯内容页靠面包屑)。④ 侧栏可收起(顶栏入口)。⑤ 切屏淡入过渡 + 动效 token(`--t-fast/.mid/.slow`)。
- **文件**:`App.tsx`、`nav/WorkspaceNav.tsx`、`session/LeadTab.tsx`、`board/NeedsYouView.tsx`、新建 home 容器。
- **验收**:进 app 默认落控制台;dock 只在工作界面;纯内容页单栏无冗余;收起/展开顺滑;切屏不硬切。参照原型 `app.jsx` / `screen-home.jsx` / `shell.jsx`。
- **风险**:🟡 中(动到主壳路由与默认界面)。
- **依赖**:无硬依赖;建议在 M-A/B 之后。

### M-D · 看板 → 信任仪表盘
- **做**:`ThreadBoard` 卡改 **acceptance 信号**(tests x/y · 类型 · 契约 · review)+ 可展开溯源;去「拖卡为主」,改自动流转 + 动作(批准/回答/打开/评审/合并)。工作区加**按仓 swimlane** 暴露热点仓。
- **文件**:`board/ThreadBoard.tsx`、`board/WorkspaceKanban.tsx`、新建 swimlane 视图;新原子 `Signal`。
- **验收**:卡片信号驱动、provenance 可展开;swimlane 标出热点仓。参照原型 `screen-board.jsx`。
- **风险**:🟡 中。
- **依赖**:🔗 **轨 B**:契约一致 / review-agent 信号、热点仓争用计算(否则信号/争用是占位)。

### M-E · 会话工作台(§4.3)+ Scope wow
- **做**:会话 7 态状态机(连接/运行/待输入/待审批/注入/暂停/退出,body 随态变)+ 键位归属 + **注入仲裁状态机** + 审批条 + 按仓 diff + 受保护合并 gate + PR/CI 条 + Inspect 逃生舱。独立 **Scope 拆解**界面(编织 lanes + 依赖顺序 + 唯一人工 gate)。
- **文件**:`session/SessionView.tsx` `Transcript.tsx` `DiffPanel.tsx` `DiffView.tsx`、新建 Scope 界面、`pty` 输入仲裁接线。
- **验收**:逐条过 §4.3(键位只截 ⌘ 前缀、焦点唯一、审批回写 y/n、注入 bracketed paste);Scope 可纠正写集合一键物化。参照原型 `screen-session.jsx` / `screen-scope.jsx`。
- **风险**:🟡 中(PTY 交互 + 合帧)。
- **依赖**:🔗 轨 C 性能(TUI 合帧)。

### M-F · 新模块 + 系统层
- **做**:首用流(onboarding)、设置/有效配置预览、弹窗体系(新建 issue/加仓/**确认合并 gate**/删除清理)、Toast/通知(+ OS 通知)、错误边界(失败可读)、状态覆盖(空/加载/错误)、响应式。
- **文件**:新建 onboarding / settings 界面、`components/ui/Dialog.tsx`(对齐现有 `dialogs.tsx`)、toast、`ErrorBoundary`。
- **验收**:首用 ~5min 到 scope wow;有效配置标出来源层;合并走不可逆 gate;崩溃不白屏;窄窗不挤压。参照原型对应文件。
- **风险**:🟢🟡 混合。
- **依赖**:🔗 轨 B:有效配置分层数据、不可逆边界配置。

---

## 轨 B · 后端补点(设计就绪、缺数据)

> `src/lib/types.ts` 已有 `RepoProfile` / `RepoGraph`(依赖边)/ `CheckResult` / `Proposal` / bus —— **不用重建**。下面是缺口:

| 项 | 内容 | 喂给前端 | 性质 |
|---|---|---|---|
| 热点仓争用 | 比对各 issue 的 write-scope 算重叠 + 分叉提交数 | M-D swimlane / 争用条 | 确定性逻辑,无需 agent |
| acceptance 阶梯补全 | 契约一致性检查 + review-agent 结论,归一成独立信号 | M-D 卡片信号 | 接现有验证阶梯 |
| 有效配置分层 | 团队⊕个人⊕仓 的 skills/rules 合并结果 + 来源层 | M-F 有效配置预览 | 读工具作用域合并 |
| 不可逆边界配置 | 合并受保护分支 / 生产部署 的可配置 gate | M-F 设置 + 合并 gate | 配置 + 校验 |
| issue 标识规则 | `thread.id` 已是 number;定对外**不显示编号**的展示规则 | 全局 | 约定 |

---

## 轨 C · 横切(每个 PR 顺手带)

- **a11y**:AA 对比度全量核对;全键盘可达 + focus ring;`prefers-reduced-motion` 降级(原型 styles.css 有现成规则)。
- **性能**:TUI 输出**合帧/批处理**管线(防 xterm 闪,ARCHITECTURE §6);大列表(多 issue / 长 transcript / 大 diff)**虚拟化**;动效只用 transform/opacity。
- **i18n**:EN 全量进 `en.ts`(原型只示意 chrome);图标 inline SVG → 现有 `lucide-react`。

---

## 推荐推进顺序(可发布增量)

1. **PR1 = M-A + M-B**(换肤 + 术语):纯 CSS/文案,当天可见,零风险,先拿视觉与术语收益。
2. **PR2 = M-C**(IA 重心):控制台为家 + dock 收敛 + 导航统一 + 收起 + 动效。体验骨架成型。
3. **轨 B 起步**(与 PR2 并行):先做**热点仓争用**(确定性、最易)+ **有效配置分层**。
4. **PR3 = M-D**(信任仪表盘):待轨 B 的契约/review 信号 + 争用就绪后接上。
5. **PR4 = M-E**(会话 §4.3 + Scope):交互核心 + 合帧管线(轨 C)。
6. **PR5 = M-F**(新模块 + 系统层):onboarding/设置/弹窗/toast/错误边界/响应式/状态。
7. **横切收尾**:a11y 全量、虚拟化、EN 全量、图标映射。

每个 PR 走仓库自身 hooks/CI(权威),Weft 内只做轻量 pre-PR 检查。**北极星(merge→staging→production)** 待 Task→PR 稳定后,再编排现有 CD,不自建发布系统。

---

## 我能直接帮的

- 在你 **工作树干净** 后,起 `loom-reskin` 分支、执行 **PR1(M-A+M-B)** 并自测(换肤 + i18n 术语)——风险最低、收益最快。
- 任一 milestone 的组件级实现(照原型 + HANDOFF §7 规范),或轨 B 某项的接线设计。
