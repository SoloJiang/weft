# Multica 可借鉴模式 → Weft 移植笔记

> 来源:对 [multica-ai/multica](https://github.com/multica-ai/multica) 全仓代码分析(Go server + 本地 daemon Runtime + 多端)。
> 目的:把 Multica 已经踩平的**运行时工程地基**提炼成可落地 Weft(Rust/Tauri)的清单。
> 立场:抄它的地基(worktree/CLI 探测/护栏/skills 注入),**守住 Weft 的差异化**(嵌 TUI 原生交互、预算护栏、质量闭环)——后两块 Multica 是空的,见文末。

## 架构对照(一句话)

| | Multica | Weft |
|---|---|---|
| 驱动 | **headless**:`claude -p stream-json` / `codex app-server` JSON-RPC / ACP | **PTY 嵌原生 TUI** |
| 审批 | 全 `bypassPermissions` / `--dangerously-skip`,ACP 自动 approve | automation-first 但**透传**权限请求 |
| 交互 | **禁掉 `AskUserQuestion`**,澄清丢 issue 评论 | 保留原生交互 |
| 形态 | 云 server + 本地 daemon + web/desktop/mobile | 纯本地 Tauri 桌面 |

---

## 借鉴清单总览

| # | 模式 | 价值 | 对应 Weft milestone |
|---|---|---|---|
| A | **CLI 探测 + 登录 shell 兜底 + 版本门禁** | 高 | M1/M3(driver spawn 前置) |
| B | **Poisoned 会话检测** | 高 | M5 跑飞护栏 |
| C | **三层 watchdog(idle / tool / wall-clock)** | 高 | M5 跑飞护栏 |
| D | **Repo 缓存 + per-task worktree 解耦** | 高 | M2 worktree 编排 |
| E | **TTL 分级 GC + 执行期保护 + artifact 单独清** | 中高 | M2/M6 |
| F | **Skills 三层叠加 + per-provider 原生目录注入** | 高 | M6 配置下发 |
| G | **Polymorphic actor(actor_type+actor_id)** | 中 | M2 数据模型 |
| H | **Squad 协议(角色边界硬编码 + 结构化评估留痕)** | 中高 | M5 lead/worker |
| I | **Co-author 自动归属 + PR 被动观测** | 中 | M6 |

> 所有 `file:line` 引用基于分析时的 `multica-ai/multica` 主干。

---

## A. CLI 探测 + 登录 shell 兜底 + 版本门禁 ★你额外要的

Weft 在 spawn 任何 driver 前必须知道:本机装了哪些 CLI、在哪、版本够不够。Multica 这套是踩过坑的成品。

### A.1 主探测:`exec.LookPath` 优先,bare name 才回落

`server/internal/daemon/config.go:148-205`(`LoadConfig` 里的 `probe` 闭包)

逻辑:
1. 取 `MULTICA_<PROVIDER>_PATH` 覆盖,否则用默认命令名(`claude`/`codex`/…)。
2. 先 `exec.LookPath(cmd)` —— 命中即用(快路径)。
3. **若覆盖值含路径分隔符(`/`、`\`)却找不到 → 硬失败**,不回落(用户显式 pin 了路径就别给他换个二进制)。
4. 只有 **bare 命令名** miss 时,才走登录 shell 兜底(见 A.2)。
5. Codex 特例:CLI 不在 PATH 时探测 macOS app bundle(`/Applications/Codex.app/Contents/Resources/codex`),`config.go:589-597`。

默认命令名清单(`config.go:584`):
```
claude, codex, opencode, openclaw, hermes, gemini,
pi, cursor-agent, copilot, kimi, kiro-cli, agy
```

### A.2 登录 shell 兜底:解决 GUI 启动拿不到 PATH ★核心坑

**问题**:GUI 启动的进程(Electron / Launchpad / launchctl,**Tauri 同理**)**不继承用户交互式 shell 的 PATH**。`claude` 在 Terminal 里能跑,不代表 Tauri 进程 `LookPath("claude")` 找得到 —— 因为 nvm/fnm/volta 的 multishell 前缀、Anthropic 原生安装器(`~/.claude/local/`)的路径只有 `.zshrc`/`.zprofile` 知道。

**解法**(`config.go:662-760`,`resolveAgentsViaLoginShell`):用 `$SHELL -ilc <script>` 让用户登录 shell 自己吐出每个命令的规范绝对路径。

关键实现要点(**移植时逐条照抄**):
- `-i`(interactive)+ `-l`(login):同时吃 `.zshrc` 和 `.zprofile`,用户两处都可能写 PATH。
- **脚本内先 `unalias`/`unset -f` 再 `command -v`**:否则 `alias claude=...` 会让 `command -v` 返回别名定义(不以 `/` 开头),后续就漏了真二进制(Multica 的 issue #2512)。
- **趁 shell 还活着 `cd "$dir" && pwd -P` 解 symlink**:fnm/nvm 的 multishell 目录在 shell 退出时蒸发,必须在返回前固化规范路径。
- **拿回路径后再 `exec.LookPath` 复核一次**:过滤别名残留和已失效路径。
- **超时硬上限**:`timeout 3s` + `WaitDelay 2s`(rc 文件里 `direnv hook`/`starship init`/`&` 后台进程会吊住 stdout pipe);只允许 POSIX shell(bash/zsh/sh/dash/ksh,fish 语法不同排除)。
- **懒触发**:只有 bare name miss 时才 fork shell(`sync.Once`),快路径零成本。

内联脚本(`config.go:734-758`,`buildLoginShellResolveScript`),可直接搬:
```sh
for n in claude codex opencode ...; do
  unalias "$n" 2>/dev/null
  unset -f "$n" 2>/dev/null
  p=$(command -v "$n" 2>/dev/null) || continue
  [ -n "$p" ] || continue
  case "$p" in /*) ;; *) continue ;; esac
  d=$(dirname "$p") && f=$(basename "$p") && c=$(cd "$d" 2>/dev/null && pwd -P) || continue
  printf '%s\t%s\n' "$n" "$c/$f"
done
```

### A.3 版本探测 + 最低版本门禁

`server/pkg/agent/version.go`

- `DetectVersion`:shell 出 `<cli> --version`,正则 `v?(\d+)\.(\d+)\.(\d+)` 抽 semver(容忍 `2.1.100 (Claude Code)` 这种尾注)。
- `MinVersions`(`version.go:13`):**按能力卡版本,并写明原因**——
  - `codex: 0.100.0`(`app-server --listen stdio://` 自 0.100 才有)
  - `claude: 2.0.0`、`copilot: 1.0.0`(`--output-format json` 信封自 1.0 稳定)
- **dev 构建放行**:`v0.2.15-235-gdaf0e9` 这种 `git describe` 形状直接 pass(`devDescribeRe`),不卡 `make build`。
- 版本缓存在内存,每次 register 带上;下游(如 Codex sandbox 策略)按版本决定隔离方式。

### → Weft 移植(Rust)

```
src-tauri/src/drivers/detect.rs
  fn detect_clis() -> HashMap<Tool, CliEntry>      // LookPath(which crate) → 登录 shell 兜底
  fn resolve_via_login_shell(names) -> HashMap      // std::process::Command($SHELL, -ilc, script) + 3s 超时
  fn detect_version(path) -> Option<Semver>         // <cli> --version + 正则
  fn check_min_version(tool, ver) -> Result<()>     // 按能力卡 + 原因文案(走 i18n)
```
- Rust 里 `which` crate ≈ LookPath;登录 shell 用 `Command::new(shell).args(["-ilc", &script])` + `wait_timeout`。
- 探测结果回流 SQLite,UI 的 Inspect 面板展示"本机可用工具 + 路径 + 版本";版本不够时给可读升级提示(M3 验收的"失败可读")。
- 卡版本的原因务必写进常量注释 —— Weft 同样依赖各 CLI 的特定能力(Codex 深链、Claude `--add-dir` 等)。

---

## B. Poisoned 会话检测 ★对应 M5 loop detection

**问题**:resume 一个"已坏"的会话会确定性地复现同样的失败,白烧钱。Multica 在任务结束后分类产出/错误,坏会话被标记后**强制开新会话而非 resume**。

`server/internal/daemon/poisoned.go`

三类分类器:
1. **输出侧**(`classifyPoisonedOutput`,line 70):匹配 agent 放弃的终止话术。
   - 标记:`"i reached the iteration limit"` → iteration_limit;`"put your final update inside the content string"` → fallback_message。
   - **关键防误判**:产出 > 320 字符就不判(`poisonedOutputMaxLen`)—— 真 fallback 是一句话;长产出里碰巧引用了这些词(如 code review 回复)是正常结论。宁可漏判(用户手动重跑)也不误判(把成功变失败)。
2. **错误侧**(`classifyPoisonedError`,line 110):**同时含** `"400"` + `"invalid_request_error"` → API 拒收了请求体(超大图/坏 base64/prompt 过长),会话历史已污染,每次 resume 都 400。**两个标记都要命中**:单独 `400` 太泛(工具随便报),单独 `invalid_request_error` 也可能误伤。429/5xx 是瞬时的,**应该** resume。
3. **超时侧**(`classifyResumeUnsafeTimeout`,line 132):**provider 特定**——仅 Codex 的语义僵死标记才判 resume-unsafe;普通基建超时保留 resume 指针让重试续上。

下游:坏会话的 `failure_reason` 入库,resume 查询(`GetLastTaskSession`)过滤掉它们,下个任务开新会话。

### → Weft 移植
- 在 worker 结束归一化时跑同样三类分类,结果写 Session 行的 `failure_reason`。
- resume 决策(你 CLAUDE.md 的 Claude `--resume`/Codex `codex resume`)前查 `failure_reason`,poisoned 则不带 resume_id、开新会话。
- **比 Multica 进一步**:它只判"单次会话坏了",**不判"同一 error 连续 N 次"**。Weft 的 loop detection 应再加:同一 direction 上 `failure_reason` 连续重复 ≥ N → 停 + 升级给人(确定性升级判据)。

---

## C. 三层 watchdog ★对应 M5 跑飞护栏

`server/internal/daemon/config.go:21-60` + `server/pkg/agent/agent.go:50-61`

**设计哲学**:默认**无 wall-clock 硬超时**(`MULTICA_AGENT_TIMEOUT=0`),只要还在出事件就不杀 —— 避免误杀 RFC 长文/多分钟构建(MUL-2300/3064)。靠三个独立看门狗兜底:

| 看门狗 | 触发条件 | 默认 | 关 |
|---|---|---|---|
| **wall-clock** | 绝对墙钟上限 | **0=无** | — |
| **idle** | 后端无消息 **且** 队列空 | 30min | `=0` |
| **tool 在飞** | `tool_use` 发出后无 `tool_result` 也无其他消息 | 2h | `=0` |
| Codex 语义不活跃 | 无语义活动 | 10min | env |

要点:idle 看门狗**在 tool 在飞期间不计时**(真实 build/install/test 会静默跑很久),改由 tool 看门狗兜底。`runContext`(`agent.go:56-61`):timeout>0 走 `WithTimeout`,否则纯 `WithCancel` 让语义看门狗当唯一 liveness。

### → Weft 移植
- 把"超时"拆成 idle / tool-stuck / wall-clock **三个独立维度**,而不是一个粗暴 timeout。
- **但 wall-clock 上限设成强制**(Multica 默认关,这是它的空白):作为每 thread/direction 预算的兜底硬顶。
- PTY 模式下,"有事件"= sidecar NormEvent 在流动 / PTY 有输出;据此重置 idle 计时。
- 全部可配(env 或 thread 级 settings),关闭语义保留。

---

## D. Repo 缓存 + per-task worktree 解耦 ★对应 M2

`server/internal/daemon/repocache/cache.go` + `server/internal/daemon/execenv/git.go`

- **bare clone 缓存一份**(`~/.../​.repos/<ws>/<host+org+repo>.git/`),每任务从中 `git worktree add`。
- **per-bare-repo mutex** 串行化 clone/fetch/worktree —— 绕开 git `packed-refs.lock` 并发约束;不同 repo 的操作仍并发。
- refspec 用 `+refs/heads/*:refs/remotes/origin/*`,**避免和 worktree 创建的 `refs/heads/*` 分支撞**。
- **分支名冲突 → 加时间戳重试一次**(`git.go:77-100`)。
- base ref 解析:`symbolic-ref origin/HEAD` → `origin/main` → `origin/master` → `HEAD` 逐级兜底。
- worktree 删除走 `git worktree remove --force` + `git worktree prune`(`git.go:102-`)。

### → Weft 移植
- 你 CLAUDE.md 的分支命名空间 `ws/<workspace>/<thread>/<direction>` 已经含 thread 维度(规避"同分支不能两个 worktree 检出")—— Multica 的时间戳兜底可作为二次保险。
- per-repo mutex 直接照搬到 Rust(`Mutex<()>` per bare repo path)。
- refspec 那条坑必踩,提前用上。
- "每 worktree 一份依赖"的磁盘问题接 E 节 GC。

---

## E. TTL 分级 GC + 执行期保护 ★对应 M2/M6

`server/internal/daemon/gc.go` + `diskusage.go`,默认值在 `config.go:51-66`

分级清理策略:
- done/cancelled 的任务目录:`updated_at < now - 24h` 清(`GCTTL`)。
- **孤儿目录**(无 `.gc_meta.json`):`72h` 后清(`GCOrphanTTL`)—— 兼容崩溃/降权 token 拿不到 issue 状态(404)的情况,用同一长 TTL,**防止降权 token 瞬间抹掉活动 workspace**。
- **artifact 单独清**(`12h`,`GCArtifactTTL`):issue 还开着但任务完成 12h 后,删 `node_modules`/`.next`/`.turbo` 这类**可重建产物但留代码**。默认清单保守(`dist`/`build`/`.cache`/`.venv` **不**默认清,可能含源码或 release)。
- **执行期保护**:`markActiveEnvRoot()`/`unmarkActiveEnvRoot()` 标记在跑的 env,GC 跳过。
- 安全护栏:绝不进 `.git`、不跟 symlink、不越界 taskDir、跳过 local_directory(用户自己的仓)。

### → Weft 移植
- thread/direction 删除时清 worktree(M2 验收),加这套 TTL 后台 GC 收尾孤儿。
- 执行期保护用 Weft 的 Session 状态(running 的 worktree 不清)。
- artifact 默认清单保守 + 可配,别误删 `dist/`。
- 顺带做 `disk-usage` 视图进 Inspect(operator 可见性)。

---

## F. Skills 三层叠加 + per-provider 原生目录注入 ★对应 M6 配置下发

`server/internal/daemon/local_skills.go` + `execenv/codex_skill_strip.go` / `codex_user_skills.go`

- 把**统一的 `SKILL.md`** 写进**每家 CLI 各自的原生 skill 目录**,靠各 CLI 原生发现机制加载:

| Provider | 目录 |
|---|---|
| claude | `~/.claude/skills/` |
| codex | `$CODEX_HOME/skills/` 或 `~/.codex/skills/` |
| copilot | `~/.copilot/skills/` |
| opencode | `~/.config/opencode/skills/` |
| cursor / kiro / pi / openclaw | 各自 `~/.<tool>/...skills/` |

- **三层解析(后者覆盖前者同名)**:
  1. builtin(教 agent 怎么用平台本身,不注入运行时,作为 context)
  2. workspace 分配的 skill(注入,**同名覆盖用户本地**)
  3. 用户本地已装 skill(CLI 原生发现)
- 名字 sanitize(小写+折叠空格)做冲突检测;workspace 版权威。
- **Codex 坑**(`codex_skill_strip.go`):Codex CLI 0.114 的 TOML parser 拒绝 plugin 条目(缺 `path`),得从 per-task `config.toml` 剥掉整个 `[[skills.config]]` 数组 —— Codex 反正会从 `CODEX_HOME/skills/` 原生发现写入的文件。
- 大小护栏:单文件 1 MiB、单 skill 8 MiB / 128 文件;二进制文件跳过(避免 PG TEXT 编码问题)。

### → Weft 移植
- 你 M6 的"有效配置预览标出 skill 来自团队/个人/仓哪层" = 这个三层模型的 UI 投影。直接用 builtin < team(workspace)< 个人(本地)< 仓库,标注来源层。
- per-provider 目录映射照搬;Codex TOML 剥离这种"provider 特定 config 适配"早做,别等踩坑。
- 注入走你的物化层(materialize),资产注入时一起写。

---

## G. Polymorphic actor 数据模型 ★对应 M2

issues / comments / 活动日志 / 订阅者所有"谁干的"字段都是 `(actor_type: member|agent, actor_id)`。agent 不特判就是一等公民。

### → Weft 移植
- Weft 的 Session `role = curator|lead|worker` 已是这个形状的近亲。把"谁产生了这条 bus 消息 / 这个 diff / 这个评论"统一成 `(actor_kind, actor_id)`,human 和各 role agent 同构,看板和 thread bus 都受益。

---

## H. Squad 协议:lead/worker 的镜子(反着学)★对应 M5

`server/internal/handler/squad_briefing.go:19-89` + `squad.go:917-1065`

Multica 的 leader:
- **硬规则:只评估 + 派发,绝不执行**。通过在评论里贴 `[@Name](mention://agent/<UUID>)` 派活,**@mention 本身就是触发信号**。
- 每轮**强制**调 `multica squad activity <action|no_action|failed> --reason "..."` 留审计(`activity_log`)。
- 角色边界写进**硬编码 briefing**(系统提示),不靠模型自觉;member 名单以可直接粘贴的 mention markdown 渲染。
- `is_leader_task` 标志 + "comment author 是不是 squad member" 判定**防自触发循环**(`comment.go:1100`)。

### → Weft 移植(取舍)
- **抄**:(a)角色边界写进**硬编码 briefing/系统提示**,不靠训练;(b)强制结构化评估留痕(Weft:`weft worker status <id> <completed|failed|blocked> --reason`,回流 thread bus)。(c)`is_leader_task` 式防自触发循环。
- **改**:Multica leader 是被动"评估器",靠 @mention 触发;**Weft 的 lead 是主动 survey→scope→spawn 的编排者**,语义更强。lead 出结构化 scope+brief(planner MCP),worker 回结构化摘要+diff stat —— 这是 Weft 比 Squad 强的地方,保持。

---

## I. Co-author 自动归属 + PR 被动观测 ★对应 M6

`server/internal/daemon/repocache/cache.go`(co-author hook)+ `server/internal/handler/github.go`

- 建 worktree 时按 workspace 开关装 `prepare-commit-msg` hook,自动给 commit 加 `Co-Authored-By: <agent>`;**装失败只 warn 不阻断**。
- **PR 被动观测**:agent 用**原生 CLI** `git push` + `gh`/API 开 PR;Multica **不**提供"调 API 开 PR"端点,只经 GitHub webhook **被动同步** PR 元数据(`github.go:189` 把 check suites 收敛成 passed/failed/pending)。

### → Weft 移植
- 完全契合你"Task→PR 为边界、不重造 CI/review"。让 worker 用原生 CLI 推 PR(不绕 hooks),Weft 只**观测**仓库现有 CI/PR 状态,不驱动。
- co-author hook 的"装失败不阻断"健壮性照抄。

---

## 不抄 / Weft 已领先(差异化护城河)

**整块丢掉(本地优先无服务端)**:云 server、heartbeat、WebSocket Hub、cloud-runtime、daemon auto-update。实时用 Tauri IPC 替 WebSocket;更新交给 Tauri updater。

**别跟(与卖点相反)**:headless 全自动 + 禁 `AskUserQuestion`。Weft 的核心恰恰是嵌 TUI 保全原生交互。

**Multica 的两个真实空白 = Weft 的护城河(被反向验证)**:
1. **预算只观测不强制**:`task_usage` 表只统计 token、**没有任何预算上限/中途熔断**(`scheduler/jobs_task_usage.go` 仅小时级汇总)。→ Weft CLAUDE.md 的"每 thread/direction 预算上限"做出来即差异化。
2. **完全没有交付前质量门**:任务只要不 `failed` 就算 done,**不跑 lint/test/CI、无确定性升级判据**(只有事后 failure 分类 + poisoned 检测)。→ Weft "acceptance 可执行化 + worker 完成=检查绿 + 验证阶梯 + 确定性升级判据"直接领先一个身位。

---

## 关键 file:line 索引

| 模式 | 文件 | 关键符号 |
|---|---|---|
| CLI 主探测 | `server/internal/daemon/config.go:148-205` | `LoadConfig` / `probe` |
| 命令名清单 | `server/internal/daemon/config.go:584` | `defaultAgentCommandNames` |
| 登录 shell 兜底 | `server/internal/daemon/config.go:662-760` | `resolveAgentsViaLoginShell` / `buildLoginShellResolveScript` |
| Codex app bundle | `server/internal/daemon/config.go:589-597` | `codexDesktopAppBundlePaths` |
| 版本门禁 | `server/pkg/agent/version.go:13` | `MinVersions` / `CheckMinVersion` / `parseSemver` |
| Poisoned 检测 | `server/internal/daemon/poisoned.go:70-145` | `classifyPoisonedOutput/Error/ResumeUnsafeTimeout` |
| 三层 watchdog | `server/internal/daemon/config.go:21-60` | `DefaultAgentIdleWatchdog` / `ToolWatchdog` |
| runContext | `server/pkg/agent/agent.go:50-61` | `runContext` |
| Repo 缓存 | `server/internal/daemon/repocache/cache.go` | `Cache.Sync` / `CreateWorktree` |
| worktree git | `server/internal/daemon/execenv/git.go:77-135` | `setupGitWorktree` / `removeGitWorktree` |
| GC | `server/internal/daemon/gc.go` | `gcLoop` / `shouldCleanTaskDir` |
| Skills 注入 | `server/internal/daemon/local_skills.go` | `localSkillRootForProvider` |
| Codex skill 剥离 | `server/internal/daemon/execenv/codex_skill_strip.go` | `stripSkillsConfigEntries` |
| Squad 协议 | `server/internal/handler/squad_briefing.go:19-89` | 操作协议 briefing |
| Co-author hook | `server/internal/daemon/repocache/cache.go` | `prepare-commit-msg` 注入 |
| GitHub PR 同步 | `server/internal/handler/github.go:189` | check suite 收敛 |
