# M2 数据模型 + worktree 编排 — 设计文档

> 项目:weft。架构见 `多仓多工具会话编排器-架构设计与可行性.md`;开工约束见 `CLAUDE-CODE-启动包.md`;产品/设计基线见 `PRODUCT.md` / `DESIGN.md`。
> 前置:M1 已完成(单工具端到端 + resume,见 `2026-06-05-m1-vertical-slice-design.md`)。
> 本文档只覆盖 **M2**。架构与技术栈已锁定,不重新讨论。

## 状态

- 日期:2026-06-05
- 范围:M2(数据模型 + worktree 编排),**后端 + 测试 + 无人值守验证为主**
- 起点:把 M1 的"临时 demo 仓 + /tmp 一次性 worktree"升级为**持久化的工作区模型 + 稳定 worktree 生命周期**,支撑并行多 thread / 多 direction。

## 目标(M2 验证什么)

> 启动包 M2 验收:① 一个 thread 下建 2 个 direction(不同仓/不同分支)互不干扰;② 删除 thread 能清理其全部 worktree;③ 同一仓被两个 thread 各开一个 worktree 不冲突。

核心是把"逻辑工作区"落成可持久、可复现、并行安全的物化层,且 **worktree 路径稳定**(resume 强依赖 cwd,M1 已证)。

## 范围

**In**

- SQLite 持久化:Workspace / RepoRef / Thread / Direction / Session / Worktree 数据模型 + 仓储层。
- 持久化 worktree 生命周期:创建 / 列出 / 删除 / 清理(prune),分支命名空间含 thread 维度。
- 按仓 diff(on-demand `git diff`/`status`,非旁路事件——旁路归一化是 M3)。
- 重构 M1 的 `open_session`:不再现造 demo 仓,而是在某 direction 已物化的 worktree 上起会话。
- 测试:git/worktree 操作、scope→物化映射、数据模型仓储的单元/集成测试;经 dev MCP bridge 的无人值守端到端验证。

**Out(M2 不做,留给后续)**

- 精致的导航 UI(workspace ▸ thread ▸ direction 树、diff 视图的成品质感)——**单独走一次 `$impeccable craft` 按 DESIGN.md 实现**,直接消费 M2 的后端命令,不建临时 UI、不返工(见"UI 边界")。
- 多 driver(Codex/OpenCode)、旁路事件归一化(M3)。
- 完整交互层 / 审批条 / 注入(M4)。
- scope 懒物化 UI、配置物化/下发(M5)。
- thread bus / coordinator(M6)。
- 每 worktree 依赖的懒装/链接共享(只记录成本,不在 M2 解决)。

## 数据模型(SQLite)

> 命名:本产品 **Thread = 工作线**(含多个原生 session),与各 CLI 内部的 session/thread 不是一回事。

```
workspace(id, name, slug, created_at)
repo_ref(id, workspace_id, name, slug, local_git_path, base_ref, default_tool)
thread(id, workspace_id, title, slug, type, status, created_at)         # status: active|paused|archived
direction(id, thread_id, name, slug, tool, branch, created_at)          # branch = ws/<ws.slug>/<thread.slug>/<direction.slug>
direction_repo(direction_id, repo_id, role)                            # role: write|read ; 每仓在该方向的角色
worktree(id, repo_id, direction_id, branch, path, created_at)          # 物化记录;path 稳定唯一
session(id, direction_id, repo_id, tool, cwd, native_session_id, status, created_at)
```

要点:
- **RepoRef 按 .git 引用**(`local_git_path` + `base_ref`),非拷贝;一个仓可被多个工作区/方向重叠引用。
- **direction_repo** 承载 scope(write/read)。M2 只对 write 仓建 worktree;read 仓的只读挂载留到 M5(M2 先不挂)。
- **worktree.path 稳定**(见下),`session.cwd = worktree.path`。
- 关系完整性:删 thread → 级联删 direction / direction_repo / worktree(并物理清理)/ session。

技术:**SeaORM**(async-first ORM,底层即 sqlx;自带 migration、entity、关系映射)。理由:既要 ORM 的开发体验(实体/关系/级联),又保留 sqlx 的可靠性,且与 Tauri 的 async 运行时天然契合。`store/` 下放 entity 定义 + migration + 仓储封装;后端可独立测(SQLite,可用内存库跑单测)。DB 文件:`~/.weft/weft.db`。

## 承重决策:worktree 物化到稳定持久目录(非 /tmp)

M1 把 worktree 放在 `$TMPDIR/weft/<nanos>`——一次性、会被清理,**不满足 resume 的 cwd 稳定性**。M2 改为持久根:

```
~/.weft/worktrees/<workspace.slug>/<thread.slug>/<direction.slug>/<repo.slug>/
```

- **分支**:`ws/<workspace.slug>/<thread.slug>/<direction.slug>`,每个 write 仓在自己的对象库里用这条分支开 worktree(共享对象库,零拷贝)。
- **并行安全**:分支含 thread+direction 维度 → 两个 thread 写同一仓 = 两条不同分支 = 两个 worktree,绝不撞"同分支双检出"。
- **slug 规则**:name → 文件系统安全 + git-ref 安全的 slug(小写、`[a-z0-9-]`、去重加短 id 后缀);路径与分支都用 slug。
- **cwd 一致性**:路径一旦写入 `worktree.path` 即不可变;canonicalize 后用于 Claude 的 encoded-cwd(M1 已证 `/`、`.` → `-`,且要先解析 symlink)。

## 模块与命令(后端)

复用并扩展 M1 的 `src-tauri`:

```
src-tauri/src/
  store/        # NEW: sqlx schema + migrations + 仓储(workspace/thread/direction/repo/worktree/session)
  git.rs        # 扩展:list_worktrees、repo_diff/status、稳健 remove+prune
  materialize.rs# NEW: scope(write/read) → 建 worktree + 写 worktree/session 记录
  claude.rs     # 不变(encoded-cwd / capture)
  pty.rs        # 重构:open_session 接收 direction_id+repo_id,用已物化 worktree 的 cwd;支持多会话(M1 是单会话)
  batch.rs      # 不变
```

**Tauri 命令(M2)**:
- `create_workspace(name)` / `list_workspaces()`
- `add_repo_ref(workspace_id, local_git_path, base_ref?)`(校验是 git 仓,取默认分支为 base_ref)
- `create_thread(workspace_id, title, type)` / `list_threads(workspace_id)` / `delete_thread(thread_id)`(级联清理所有 worktree)
- `create_direction(thread_id, name, tool, scope: {repo_id: "write"|"read"})` → 对 write 仓批量物化 worktree,落记录
- `list_worktrees(thread_id?)` / `repo_diff(worktree_id)`(返回 files + 增删行)
- `open_session(direction_id, repo_id)` → 在该 (direction,repo) 的 worktree 起原生会话(替代 M1 的 demo 路径);沿用 M1 的 resume/capture
- `resume_session(session_id)` / `write_pty(session_id, data)` / `resize_pty(session_id, …)` / `kill_session(session_id)`
  - **多会话化**:M1 的 PtyState 是单会话;M2 改为 `HashMap<session_id, Active>`,命令按 session_id 寻址(为 M3 并排多会话铺路)。

## UI 边界(M2 故意保持最小)

M2 的验收是**正确性**(数据模型 + worktree 生命周期 + 并行安全),不是 UI 质感。为避免"先做功能 UI 再返工质感",M2 **不建临时导航 UI**:

- M2 交付**后端命令 + 测试 + 经 dev MCP bridge 的无人值守端到端验证**(沿用 M1 建立的 `webview_execute_js` / `__TAURI_INTERNALS__.invoke` 设施直接调命令断言)。
- 真正的 **workspace ▸ thread ▸ direction 导航树 + 按仓 diff 视图**,作为**紧接其后的 `$impeccable craft workspace-nav` 一次性按 `DESIGN.md` 实现**(shadcn 重调味、深色高密度、状态 chip、动效衔接),直接消费 M2 命令——一次做到位。

> 即:M2 = 正确的地基;紧随其后的 craft pass = 长在地基上的成品 UI。两者不重叠、不返工。

## 测试

- **单元**:slug 生成(文件系统 + git-ref 安全、去重);branch 命名;scope→物化映射(write 建/read 不建/none 不进);仓储 CRUD 与级联删除。
- **集成(真 git)**:① 一个 thread 两个 direction(不同仓不同分支)各自 worktree 互不干扰;② delete_thread 清掉其全部 worktree 且 `git worktree prune` 干净;③ 同一仓被两个 thread 各开 worktree 不冲突(两分支两路径);④ repo_diff 反映真实改动。
- **端到端(dev MCP bridge,无人值守)**:create_workspace → add_repo_ref → create_thread → create_direction(write 多仓)→ list_worktrees 断言路径/分支 → open_session 在某 worktree 起会话 → resume 接回 → delete_thread 清理。

## 关键回归点

- worktree 路径**稳定且 canonical**(resume 依赖);删除走 `git worktree remove --force` + `prune`,不留悬挂。
- 分支命名空间**必须含 thread+direction 维度**,否则同仓并行撞分支报错。
- **接线不进 canonical 仓**(架构 §2.1):M2 只建 worktree + 落本地 DB 记录,不往受版本管理的仓内配置写任何东西。
- 多会话 PtyState 改造后,M1 的单会话 resume/capture 行为不回退(回归测一遍)。

## 完成定义(M2 Done)

- 数据模型落 SQLite,仓储层有单元测试。
- worktree 生命周期(建/列/删/prune)+ 按仓 diff 命令齐备并通过集成测试。
- 启动包 M2 三条验收(两 direction 互不干扰 / 删 thread 清理 / 同仓两 thread 不冲突)逐条通过。
- 经 dev MCP bridge 的端到端串联跑通并断言。
- 改动说明随交付。后续 `$impeccable craft workspace-nav` 作为独立 UI pass 排期。

## ✅ M2 实测结论(2026-06-05,全部通过)

后端按 10 个 TDD 任务实现,SeaORM ORM(`Schema::create_table_from_entity` 从实体派生迁移、零 SQL)。

**自动化测试(可重复运行):**
- 12 个 lib 单测(slug、paths、migration、仓储 CRUD+级联删除)全过。
- `m2_git` 集成测试(worktree 列举 + 按仓 diff)过。
- `m2_worktree` 集成测试(三条验收场景 ①②③)**连跑两次都过**——经 `WEFT_HOME` 隔离修复后不再依赖干净的 `~/.weft`。

**经 dev MCP bridge 的真实 IPC 端到端(隔离 `WEFT_HOME`):**
- `create_workspace`(slug 去重 e2e/e2e-2)→ `add_repo_ref`(校验 git 仓、派生 base_ref)→ `create_thread` → `create_direction`(**materialize 副作用**)→ `list_worktrees`:worktree 物化在 `<home>/<ws>/<thread>/<dir>/<repo>`,分支 `ws/<ws>/<thread>/<dir>`,`git worktree list` 确认注册。
- `delete_thread`:目标 thread 的 worktree 从盘上 + git + DB 级联移除,另一 thread 存活。
- `open_session(direction,repo)`:在已物化 worktree 上创建 session 行(cwd 正确)并 spawn 原生 claude(live resume 机制与 M1 已证一致)。

**开发期设施:** `WEFT_HOME` 环境变量可覆盖 weft 数据根(测试隔离 + 用户可迁移数据);debug-only `tauri-plugin-mcp-bridge` 支撑无人值守 IPC 验证。

**代码审查必修(已修,commit `6bf3b15`):** 删除 worktree 时一并 `git branch -D` 删掉 `ws/...` 命名空间分支(否则每次删 thread 在用户仓累积悬挂分支,违背零累积);`now()` 时钟兜底防 panic;`cleanup_worktrees` 不再吞 git 错误。

### 延后的 follow-up(审查发现,不阻塞 M2 正确性)

- **物化非原子**:`materialize_direction` 先建 worktree 再插 DB 行,二者之间崩溃会留下"有 worktree 无 DB 行"的孤儿。修法:启动时用 `git worktree list` 对账 DB,或先记意图再建。
- **时间戳**:`created_at` 当前存 epoch 秒字符串(注释写 RFC3339)。改存真正 RFC3339 或 integer epoch。
- **外键无索引**:`workspace_id/thread_id/direction_id/repo_id` 上加索引迁移(数据增长前)。
- **`SessionInfo.repo` 字段名误导**:当前存的是 worktree 路径而非仓名,后续 UI craft pass 时校正。
