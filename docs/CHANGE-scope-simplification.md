# 变更说明 — scope 与确认模型简化(给已在实施的代码)

> 适用对象:已按"早期版本(read/write/none 三态 + scope 确认步 + spawn 审批)"开始写的实现。本文件只讲**这次的关键变更与迁移动作**,其余设计不变。
> 一句话:**别管"读",只管"写";确认只发生在"写 trigger",不是每次编辑。**

---

## 1. 变更总览(Before → After)

| 维度 | Before(旧版,你可能已实现) | After(新版,照此改) |
|---|---|---|
| scope 模型 | 每仓三态 `write / read / none`,都受管 | **只有 write set 受管**(会改哪些仓);读免费、无状态 |
| read 仓 | 建"只读 worktree / 只读挂载" + 强制只读 | **不建任何东西**;agent 直接读即可 |
| none 仓 | 显式"不挂"状态 | 不存在该状态;没引用就是没引用 |
| 只读保证 | per-repo 只读 enforcement | **一条全局规则**:工具写沙箱限定在 worktree 集合 |
| 上下文来源 | read 仓挂载提供 | Curator 地图 + brief 的 contract+pointers;真要 browse 才读 |
| 确认时机 | scope 三态确认步 + spawn 审批(多处 gate) | **单次"写 trigger"**:Approve & run(规划→开始写),其余全自动 |
| 每次编辑 | (隐含可能要管) | **交工具自带权限**(Codex/Claude,用户调,可全自动),Weft 只透传 |
| scope 变化 | none→writable 提升 | **auto-promote**:要改不在 write set 的仓 → 自动建 worktree = 一次新写 trigger |

---

## 2. 数据模型 diff

```diff
 Thread.plan:
-  scope: { [repoId]: write | read | none }
+  writeRepos: [repoId]        // 唯一受管:会改的仓

 Direction:
   writeRepoIds: [repoId]       // 保留:本方向会改的仓 → worktree+分支
-  readRepoIds:  [repoId]       // 删除:读不需要登记
```

`Session.role / surface`、`Thread.task / type / leadAgent`、worktree/分支命名(`ws/<workspace>/<thread>/<direction>`)等**均不变**。

---

## 3. 物化(materialize)diff

- **删除**:为 read 仓创建只读 worktree / 只读挂载的代码路径。
- **保留**:只为 **write set** 仓创建 worktree + 分支。
- **新增/改**:把每个 worker 会话的**写沙箱限定在它的 worktree 集合**
  - Codex:`sandbox_workspace_write` 的 `writable_roots` = 该会话的 worktree(们);
  - Claude:permission 规则,只允许写 worktree 路径;
  - 效果:没 worktree 的仓**天然写不进、不会污染真仓**——读仍自由。
- **跨仓上下文**:不再靠挂载 read 仓;改为在 brief 注入 `interface-contract + pointers`,需要浏览时 agent 自行读文件。

---

## 4. 确认 / 编排 diff

- **删除**:① "给每个仓选 write/read/none"的 scope 确认 UI;② spawn worker 前的单独审批 gate;③ 规划/编排过程中的任何 Weft 确认。
- **替换为**:**单次「写 trigger」确认**
  - 主 trigger:从"只读规划"转入"开始写"(即将 spawn worker / 动手改)→ 人按一次 **Approve & run**;
  - 扩张 trigger:执行中 auto-promote 到一个新仓(开始写它)→ 再确认一次(低频,每仓一次);
  - 该确认**默认开,可关成全自动**。
- **per-edit / 命令的写**:不由 Weft 确认,**交工具自带权限**(用户在自己 CLI 里配,可 on-request 也可 full-access),Weft 仅透传其审批 UI。
- **其余**:automation-first 不变——规划、派发、驱动、验证全自动;只有"写 trigger"和工具自身权限是人会被触达的点。

---

## 5. 迁移 checklist

- [ ] 数据模型:`scope{write|read|none}` → `writeRepos[]`;移除 `Direction.readRepoIds`。
- [ ] 物化:删掉 read 仓的只读 worktree/挂载逻辑;只对 write set 建 worktree。
- [ ] 写隔离:把工具写沙箱(writable_roots / permission)限定到 worktree 集合(全局一条规则)。
- [ ] 上下文:read 挂载 → brief 的 `interface-contract + pointers`;按需读文件。
- [ ] 确认:移除 scope 三态确认 + spawn 审批;实现单次 **Approve & run(写 trigger)** + auto-promote 触发的新 trigger;两者可配置关闭。
- [ ] per-edit:确保走工具原生权限,不要在 Weft 侧拦截每次编辑。
- [ ] 看板:卡上的 `r:`(read)标记可去掉;只留 write 仓 / diff;`Needs you` 含"写 trigger 待确认"。

---

## 6. 不变的部分(无需改)

Curator(仓库地图 / Repo Profile + 依赖图)、lead/worker 角色与 Brief 契约、thread bus + coordinator、自动化质量闭环(可执行验证 + 升级判据 + 护栏)、交付边界(Task→PR)、surface 解耦 + Open in app、agent-first 两级看板、i18n、产品化屏蔽原则——**全部不变**。

---

## 7. 为什么这么改(一句话)

读是无害的,不需要任何机制;**唯一需要"隔离 + 确认"的就是写**。把三态 + 一堆 enforcement/隔离/快照塌缩成「**一个 write set + 一条写规则 + 写 trigger 确认 + auto-promote**」,显著降复杂度,且与 automation-first 完全一致。
