# Thread Bus (coordination) — 设计文档

> 项目:weft。架构见 `多仓多工具会话编排器-架构设计与可行性.md` §4.2;产品定位见 `PRODUCT.md`(核心 = 编排 + **协调**,非看执行)。
> 前置:M1/M2 完成,异构三家 driver 已打通,UI 有 thread 看板。
> 本文档覆盖 **thread bus(协调内核)** 的 v1 设计。

## 状态
- 日期:2026-06-05
- 定位:这是产品的**灵魂特性**——让一个 thread 内的并行方向(可能跑不同工具)**互相通信、协调**,而不是各跑各的最后撞合并冲突。是差异化里"无人做全"的那块。

## 目标
让 thread 内不同 direction 的 agent 会话能:**互发消息 / 广播 / 读收件箱 / 读写共享线程状态 / 宣告接口变更**;并让产品在消息到达时**唤醒**目标会话去读。跨工具(Claude/Codex/OpenCode 都支持 MCP)。

## 两层机制(架构 §4.2)

### 被动层(v1 一并做,零集成、最稳)
Thread 级共享:`<viewRoot>/.thread/` 状态目录 + `PLAN.md` 黑板,物化时注入每个 direction 的 worktree(或一个共享只读挂载)。一方写、另一方下回合读。robust、tool-agnostic,但只在对方下次读时感知。

### 主动层(v1 核心:结构化消息总线)
weft 起一个**本机 MCP server(thread bus)**,挂到该 thread 所有会话,暴露工具。agent 调 MCP 工具收发,无需共享可写目录。

## ✅ Spike 结论(2026-06-05,承重风险全绿)

用一个最小 Node HTTP MCP bus(`bus_post`/`bus_inbox`,in-memory)实测三家:
- **Claude**:`--mcp-config <ephemeral.json>`(http transport)→ 调 `bus_post` 成功;**叠加性证实**——子仓自带 `.mcp.json`(repo_own)与 weft_bus 同时生效,未覆盖。
- **Codex**:`codex exec -c mcp_servers.weft_bus.url=<url>` → 日志 `mcp: weft_bus/bus_post completed`,消息入收件箱。
- **OpenCode**:项目 `opencode.json` 的 `mcp: { weft_bus: { type:"remote", url } }` → 调 `weft_bus_bus_post` 成功;`opencode mcp list` 显示**全局 codegraph + 项目 weft_bus 共存**(叠加证实)。

→ **单个本机 HTTP MCP bus + 临时叠加注入,三家全部可行。** 设计成立,进入 v1 实现。
唯一实现细节:**OpenCode 注入须深合并进子仓 opencode.json(不可覆盖)**;Claude/Codex 用启动 flag 天然不碰文件。

## ✅ v1c 实测结论(2026-06-05,自动唤醒,协调闭环闭合)

`BusRegistry` 在 post/broadcast 时经 channel 发 `Wake{thread,dir}`;`Active` 记录 `direction_id`;`PtyState::wake_direction` 注入;`coordinator` 任务(独立 OS 线程,在 Tauri `setup` 启动)消费 Wake、按方向 8s 限流、调 `wake_direction` 注入"读收件箱"提示。实测:
- 真实 app 的 claude 会话空闲在主输入框时,从假 dir curl `bus_post` 到该方向 → claude 输入行**自动**出现 "You have new messages on the thread bus. Call the bus_inbox tool to read them."(无需人手动 nudge)。
- 限流:8s 窗口内 3 条快速消息 → 仅注入 1 次。
- 依赖 TUI 对 mid-turn 输入的排队(提示排到当前回合后执行),非精密空闲检测——诚实约束兑现。被动 `.thread/` 层 + 焦点感知抑制留 v1d。

`bus(agent 主动) + coordinator 唤醒(推送) = 准实时`,如架构 §4.2 所述,现已打通。

## ✅ v1b 实测结论(2026-06-05,协调面板上线)

UI 协调面板(`src/board/CoordinationPanel.tsx`)+ 两个命令(`thread_messages` 读非破坏性时间线、`bus_post_human` 人以 "you" 身份发/广播)+ 每方向 nudge(复用 write_pty)。实测:
- 看板右栏渲染 bus 时间线:backend(dir2)`announce_interface_change`("POST /checkout now requires an idempotency-key header",带喇叭标记)+ `bus_post` 到 frontend,名字解析为 backend/frontend/all。
- 人经 `bus_post_human` 发到 frontend → frontend 收件箱含 `from:"you"` 与 `from:"2"`(agent 可读)。
- 自动 coordinator 唤醒 + 被动 `.thread/` 层留 v1c(自动注入需可靠空闲检测,否则会在 agent 推理中途写 stdin)。

## ✅ v1a 实测结论(2026-06-05,bus 核心打通)

后端按 6 个 TDD 任务实现并验证(`docs/superpowers/plans/2026-06-05-thread-bus-v1a-core.md`):
- BusRegistry/state 单测(post/broadcast/inbox/state/线程隔离)全过。
- `bus_http` 集成测试:真实 HTTP(reqwest)→ MCP handler → 两 direction 一发一收路由正确。
- 注入单测三家全过,含**"子仓自带 opencode.json 时,注入叠加保留 repo_own"**。
- **app 端到端**:启动时起 bus(临时端口),`open_session` 为 claude 方向注入 `--mcp-config` 指向 `/bus/<thread>/<dir>/mcp`;真实 app 会话的 claude 启动后**连上注入的 bus 并注册成员**(从假 dir broadcast → claude 方向收到,证明已连接 + 路由)。

→ **共享 HTTP MCP bus + spawn 时叠加注入,在真实 app 中打通。** v1b(UI 协调面板 + coordinator 唤醒 + 被动 .thread/ 层)接续。

## 承重决策(已核验)

**bus = 单个本机 HTTP MCP server(随 app 起,全局一个进程),按 URL 路径参数区分 thread + direction。**
- **不是每 thread 一个进程**。一个 server 持有 `map: threadId → BusState`,按请求路径路由。进程生命周期简单,无每 thread 进程管理。
- 多 agent 共享 → 需要 HTTP/SSE transport(stdio 是 1:1)。**Claude 确认支持 `--transport http`**;Codex/OpenCode 的 HTTP 支持在 spike 确认,不行则退化为"每会话一个 stdio 代理 → 同一后端状态"。
- **身份从 URL 带**:`http://127.0.0.1:<port>/bus/<threadId>/<directionId>`。agent 从自己注册的 URL 即带 thread + 身份,server 端按路径判定收发件箱,**无需 agent 自报身份**(防伪、零信任 agent 输入)。

### 注入原则:**叠加(merge),绝不覆盖** —— 子仓自带配置必须保全

worktree 里 check 出来的就是子仓自己的 `.claude/` `.agents/` `.mcp.json` `opencode.json`。weft 注入 bus 只能**叠加**在其上,既符合 §2.1(不写 canonical 仓)又符合产品原则"镜像用户工具、绝不覆盖":

- **优先纯叠加的启动注入(不碰任何文件)**:
  - Claude:`--mcp-config <ephemeral.json>` —— 与项目 `.mcp.json` + 用户配置 **merge**,只增不改。
  - Codex:`-c mcp_servers.weft_bus.url=<url>` —— 只设一个嵌套键,保留其余 `mcp_servers` 与 `config.toml`/`AGENTS.md`。
- **不得已要写文件时(若 OpenCode 无纯启动注入)**:**读子仓现有配置 → 深合并 weft 的 bus 条目 → 写到 opencode 也读、优先级叠加、但不改动仓内已提交文件的位置**(worktree-local + gitignore + 用完清)。绝不直接覆盖仓里的 `opencode.json`。
- **spike 必须显式验证**:在一个**已自带 `.mcp.json` / 自己的 MCP server / skills** 的子仓里注入 bus 后,子仓原有的 MCP server 与 skills **仍然生效**(叠加而非覆盖)。这条不达标则该工具退化到被动层。

## bus 工具集(v1)
| 工具 | 作用 |
|---|---|
| `post(to, text)` | 给某 direction 的收件箱投递 |
| `broadcast(text)` | 投递给 thread 内所有其他 direction |
| `inbox()` | 读并清空自己的未读(返回 from/text/ts 列表) |
| `thread_state_get()` | 读共享线程状态(JSON blob) |
| `thread_state_set(patch)` | 合并写共享线程状态 |
| `announce_interface_change(summary)` | = broadcast 的语义糖,标 type=interface |

`ask(target, q)`(请求-响应)留 v2(需要会话级关联)。

## 诚实约束 & 协调器唤醒
- agent 按回合行动,MCP 对其是"主动 poll"。真正"推送"需 **coordinator**:监听旁路/状态,在消息到达且目标空闲时,往目标 PTY **注入一个新 turn**(`write_pty` 一段 "你有新消息,请调用 inbox()")。`bus(agent 主动) + coordinator 唤醒(推送) = 准实时`。**无法打断进行中的推理**。
- **v1 范围**:做 bus 工具 + 收发/状态 + UI 面板 + 人也能发;**coordinator 自动唤醒做"基础版"**(目标空闲时注入提示),不做复杂调度。注入仲裁复用 4.3 状态机(人打字/回合中不注入)。

## 用途定位(契合产品核心)
总线主要传**契约/接口/进度/请求**,不传大块代码;各方向各拥不同仓的 worktree、不互相改文件,天然避开写冲突。这正是"协调让并行收敛"而非"看执行"。

## 数据 & 模块(后端)
```
src-tauri/src/bus/
  mod.rs        # 每 thread 一个 BusState(inboxes: dir->Vec<Msg>, state: Json, members)
  server.rs     # 本机 HTTP MCP server(JSON-RPC + SSE);路由 /bus/<thread>/<dir>
  mcp.rs        # MCP 协议:initialize / tools/list / tools/call;工具 schema
  inject.rs     # 在物化/spawn 时为各工具生成临时 MCP 注入配置(§2.1)
```
- 复用 rmcp(官方 Rust MCP SDK)实现 server 侧,减少手写 JSON-RPC/SSE。
- BusState 在内存(可选落 `.thread/` 持久)。Msg = {from_dir, text, ts, kind}。
- 与 pty.rs 集成:spawn 时按 tool 注入 bus 的 MCP 配置(SpawnSpec 增加 `mcp: Option<BusRef>`)。

## UI(前端)
- Thread 看板加一个**协调条/面板**:按时间线显示 bus 消息(from 哪个 direction、kind),人可在此发 `post/broadcast`。
- direction 卡片上显示"未读消息数"小标。
- coordinator 唤醒做成可见事件(注入了什么、给谁)。

## 测试 & 验证
- **spike(先行,承重)**:起最小 bus(一个工具 `inbox/post`)→ 用 `--mcp-config` 注册给一个真实 claude 会话 → 让 claude 调 `post`/`inbox` 成功 → 再验 Codex(`-c`)、OpenCode(worktree json)能注册并调用。
  - **叠加验证(必做)**:在一个**已自带 `.mcp.json` 且声明了自己一个 MCP server** 的子仓里注入 bus,确认注入后 `claude mcp list`(及 codex/opencode 等价)**同时**列出"子仓自己的 server"和"weft_bus" → 证明叠加未覆盖。
  - 任一条不通则该工具退化到被动层(共享文件)。
- 集成:两个 direction(claude + codex)经 bus 一发一收;thread_state 读写一致;coordinator 在目标空闲时注入唤醒。
- 经 dev MCP bridge 端到端:UI 面板显示消息、人发消息、未读标。

## 范围边界
**In(v1)**:被动层(.thread/ + PLAN.md 注入)、主动 HTTP MCP bus(post/broadcast/inbox/thread_state/announce)、临时注入三家、基础 coordinator 唤醒、UI 协调面板。
**Out(留后)**:`ask` 请求-响应、复杂自动编排/调度策略、跨 thread 协调、bus 消息持久化与回放。

## 关键风险
1. **Codex/OpenCode 的 HTTP MCP 支持未证** → spike 先验;退化方案:stdio 代理 or 仅被动层。
1b. **注入可能覆盖子仓自带配置** → 原则定为"叠加 merge,绝不覆盖";优先纯启动注入(不碰文件),必须写文件时深合并;spike 显式验证子仓原有 MCP/skills 仍生效。
2. **唤醒注入与人输入/回合的仲裁** → 复用 4.3 状态机,空闲才注入。
3. **MCP server 实现成本** → 用 rmcp 降低;v1 工具集刻意小。
4. **身份/路由正确性** → 用 per-direction URL,server 端按路径判定,避免 agent 自报。

## 完成定义(v1 Done)
- 三家会话都能注册 bus 并成功 `post/inbox`(或明确记录某家退化为被动层)。
- thread_state 跨方向读写一致。
- coordinator 基础唤醒在目标空闲时注入提示。
- UI 协调面板显示消息 + 人可发 + 未读标。
- 全程接线不进 canonical 仓。
