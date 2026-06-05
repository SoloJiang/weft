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

## 承重决策(已核验)

**bus = 本机 HTTP MCP server,每 thread 一个实例;按 direction 标识身份。**
- 多 agent 共享一个 bus → 需要 HTTP/SSE transport(stdio 是 1:1)。**Claude 确认支持 `--transport http`**;Codex/OpenCode 的 HTTP 支持在 spike 确认,不行则退化为"每会话一个 stdio 代理 → 同一 thread 后端状态"。
- **身份**:bus URL 含 direction 维度,如 `http://127.0.0.1:<port>/bus/<threadId>/<directionId>`。agent 从自己注册的 URL 即带身份,server 据此路由收发件箱,无需 agent 自报身份。
- **临时注入,不污染 canonical 仓(§2.1)**:
  - Claude:`--mcp-config <ephemeral.json>` 启动时加载(用完即弃)。
  - Codex:`-c mcp_servers.weft_bus.url=<url>`(或等价)启动覆盖。
  - OpenCode:worktree-local、gitignore 的 `opencode.json` 声明该 MCP server。
  - 这些都在物化层注入,绝不写进 canonical 仓。

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
- **spike(先行,承重)**:起最小 bus（一个工具 `inbox/post`）→ 用 `--mcp-config` 注册给一个真实 claude 会话 → 让 claude 调 `post`/`inbox` 成功 → 再验 Codex(`-c`)、OpenCode(worktree json)能注册并调用。这条不通则退化到被动层(共享文件)。
- 集成:两个 direction(claude + codex)经 bus 一发一收;thread_state 读写一致;coordinator 在目标空闲时注入唤醒。
- 经 dev MCP bridge 端到端:UI 面板显示消息、人发消息、未读标。

## 范围边界
**In(v1)**:被动层(.thread/ + PLAN.md 注入)、主动 HTTP MCP bus(post/broadcast/inbox/thread_state/announce)、临时注入三家、基础 coordinator 唤醒、UI 协调面板。
**Out(留后)**:`ask` 请求-响应、复杂自动编排/调度策略、跨 thread 协调、bus 消息持久化与回放。

## 关键风险
1. **Codex/OpenCode 的 HTTP MCP 支持未证** → spike 先验;退化方案:stdio 代理 or 仅被动层。
2. **唤醒注入与人输入/回合的仲裁** → 复用 4.3 状态机,空闲才注入。
3. **MCP server 实现成本** → 用 rmcp 降低;v1 工具集刻意小。
4. **身份/路由正确性** → 用 per-direction URL,server 端按路径判定,避免 agent 自报。

## 完成定义(v1 Done)
- 三家会话都能注册 bus 并成功 `post/inbox`(或明确记录某家退化为被动层)。
- thread_state 跨方向读写一致。
- coordinator 基础唤醒在目标空闲时注入提示。
- UI 协调面板显示消息 + 人可发 + 未读标。
- 全程接线不进 canonical 仓。
