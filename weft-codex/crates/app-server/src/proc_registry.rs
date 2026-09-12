//! 派生进程身份与登记底座 (process identity & registration base).
//!
//! ## 问题(P0:进程树失控)
//! Weft 每个会话起一个 codex app-server **直接子进程**;codex 又把用户配置的 stdio
//! MCP server 拉起。现有的 `kill_on_drop` / `child.kill()` 只到直接子进程粒度,杀掉
//! codex 后它拉起的 MCP server 被孤儿化(reparent 到 launchd/init)继续存活 = 泄漏。
//! 会话数 × 每会话数个 MCP server ⇒ 数千进程 ⇒ 撞 ulimit(≈4000)⇒ 系统级 fork 瘫痪。
//!
//! ## 实测:codex 对 MCP server 做了「双重隔离」(2026-07-21,codex-cli 0.144.3)
//! 真起 codex app-server 观察它拉起的 MCP server,发现两条身份线索都被切断:
//! 1. **各自独立进程组**:每个 MCP server 都是自己进程组的组长(pid == pgid),**不**
//!    继承 codex 的 pgid。→ 单发 `killpg(codex 组)` 收不走它们。
//! 2. **清洗环境**:codex 给 MCP server 的环境不含我们在 codex 上设的 `WEFT_INSTANCE_ID`
//!    marker(实测 8 个后代只有 codex 亲儿带,MCP server 全无)。→ env-marker 口径也失效。
//!
//! 唯一活下来的线索是 **ppid 父子链**。故本底座的口径 = **「后代闭包」**:
//!
//! ## 口径(criterion)—— 单一谓词 [`is_ours`]
//! 一个存活进程属于本实例 ⟺ **沿 ppid 上溯能到达某个登记在册的直接子进程**(即它是某
//! 直接子进程的后代或其本身)。[`count_instance_processes`] 与(T2 的)孤儿判定都**只**
//! 调 `is_ours`,故「计数口径」与「孤儿判定口径」结构上不可能漂移(硬不变量)。
//!
//! ## reap 必须树感知
//! [`reap`] 在直接子进程(codex)**还活着**时快照它的后代闭包,对闭包里**每个不同的
//! 进程组各发一次 `killpg`**(带走 codex 的组 + 每个 MCP server 各自隔离的组 + 它们的
//! 子孙),再 `wait` 收尸直接子进程。[`configure`] 仍让每个直接子进程自成进程组——保证
//! 它的组 ≠ Weft 自身的组,reap 杀组时绝不误伤 Weft 本体(另有 guard 双保险)。
//!
//! ## 诚实边界
//! 若 codex **硬崩溃**、MCP server reparent 到 init 后 ppid 链断,已无法再归属(codex 的
//! 双重隔离下无解)—— 属 T2 的「存活期周期性把后代 pgid 快照进登记表」增强。正常会话
//! 结束 / 引擎 bounce / 重启(占绝大多数)在 reap 时链完好,整树回收。
//!
//! ## fork-free
//! 逼近 ulimit 时连 `ps`/`kill` 都要 fork 会失败,故枚举与杀进程一律走 syscall
//! (`libc::killpg`、Linux `/proc`、macOS `proc_pidinfo`),绝不 shell 外化。

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};

/// 每个受管子进程携带的实例标记 env。codex 会清洗它(见模块文档),故它**不是**主
/// 口径；对调试与未来跨重启清理仍有价值。
const ENV_INSTANCE: &str = "WEFT_INSTANCE_ID";
/// 属主标记 env(便于调试 / T2 按属主定向)。
const ENV_OWNER: &str = "WEFT_PROC_OWNER";

/// 上溯 ppid 的深度上限(防病态/环状 ppid 死循环;真实进程树很浅)。
const MAX_ANCESTRY_DEPTH: usize = 64;

/// 本 Weft 进程(实例)的稳定身份。首次调用时铸一次,导出为每个受管子进程的
/// `WEFT_INSTANCE_ID`。
pub fn instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    // 用本进程 pid 作实例身份:同一时刻并发的多个 Weft 实例 pid 互异,足以区分
    // 「本实例 vs 别的 Weft 实例」。
    ID.get_or_init(|| std::process::id().to_string()).as_str()
}

/// Defines [`OwnerKind`] and every method whose correctness depends on
/// covering all variants, from ONE list of `variant => "label"` lines — so a
/// variant cannot be *defined* without also being *wired into* [`OwnerKind::all`].
///
/// This replaces the previous `next()`-linked-list approach after round-2
/// review found it didn't actually deliver that guarantee: `next()`'s
/// exhaustive `match` only forces every variant to have *some* arm, never
/// that the arm's value keeps it reachable from `all()`'s traversal. A new
/// variant given an arm like `NewKind => None` satisfies the compiler,
/// compiles clean, and leaves the old hard-coded-8-item regression test
/// green — while `all()` / `instance_owner_counts` / the dashboard's
/// "process tree" breakdown silently drop the new variant. That's the exact
/// "array literal missing an item" failure round-1 set out to close, just
/// one indirection removed. (This also corrects an overclaim in that
/// revision's comments and commit message, which read the exhaustive
/// `match` as forcing the variant to be "wired into the traversal chain" —
/// it only forces *an* arm to exist, never that the arm is correct or
/// reachable. "Compiles" was never the same guarantee as "wired in".)
///
/// Generating `all()` as a plain `vec![...]` straight from this list — not a
/// `while let` walk over a hand-linked chain — also closes the round-2
/// finding that an accidentally-cyclic chain would hang `all()` forever:
/// there is no traversal left to loop, so there is nothing to protect with
/// an iteration cap. `all()` backs the resource dashboard's 3s poll, so a
/// silent hang here would have been a real incident, not a red test.
macro_rules! owner_kinds {
    ($($(#[$doc:meta])* $variant:ident => $label:literal),+ $(,)?) => {
        /// 逻辑属主 = 谁要的这个子进程,便于按属主整树收尸。
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum OwnerKind {
            $($(#[$doc])* $variant,)+
        }

        impl OwnerKind {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(OwnerKind::$variant => $label,)+
                }
            }

            /// 全部变体,声明顺序,供 [`instance_owner_counts`] 遍历。直接由
            /// `owner_kinds!` 宏调用(见下方)的 token 列表展开成 `vec![...]`——不是
            /// 遍历一条手工维护的链,也不是另一份手写数组字面量。一个变体能不能被
            /// 定义出来,和它会不会出现在这里是**同一件事**(同一次宏展开):不存在
            /// 「变体已定义、但没接进遍历」的中间状态,也没有链式结构可供死循环。
            fn all() -> Vec<OwnerKind> {
                vec![$(OwnerKind::$variant,)+]
            }
        }
    };
}

owner_kinds! {
    /// 全局(app-scoped)codex app-server。
    GlobalAppServer => "global_app_server",
    /// 其它 / 测试。
    Other => "other",
}

/// 一个受管子进程的属主标识 `{kind, id}`。`id` 通常是 session/thread id;无自然 id 的
/// (全局/curator/探测)留空串。
#[derive(Clone, Debug)]
pub struct Owner {
    pub kind: OwnerKind,
    pub id: String,
}

impl Owner {
    pub fn new(kind: OwnerKind, id: impl Into<String>) -> Owner {
        Owner { kind, id: id.into() }
    }
    pub fn global_app_server() -> Owner {
        Owner::new(OwnerKind::GlobalAppServer, "")
    }
    pub fn other(id: impl Into<String>) -> Owner {
        Owner::new(OwnerKind::Other, id)
    }
    /// `WEFT_PROC_OWNER` 的取值,形如 `session:42`。
    fn tag(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

// ── 登记表 ──────────────────────────────────────────────────────────────────

struct Entry {
    /// 唯一登记 id(单调递增)。**摘登记按 id、不按 pid**:一个子进程死后其 OS pid 可能
    /// 被下一次 spawn 复用,若按 pid 摘登记,老登记项的 Drop 会误删掉恰好复用了该 pid 的
    /// **另一个存活**登记项(计数漏掉活着的子树)。按 id 摘则各登记项互不干扰。
    id: u64,
    pid: u32,
    pgid: i32,
    owner: Owner,
}

fn registry() -> &'static Mutex<Vec<Entry>> {
    static R: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_reg_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// 登记在册的直接子进程 pid 集合(= 后代闭包/口径的根)。
fn registered_pids() -> HashSet<i32> {
    match registry().lock() {
        Ok(v) => v.iter().map(|e| e.pid as i32).collect(),
        Err(_) => HashSet::new(),
    }
}

fn deregister(id: u64) {
    if let Ok(mut v) = registry().lock() {
        v.retain(|e| e.id != id);
    }
}

/// 登记表快照(供 T2 的 orphan sweep / 进程 gauge)。
#[derive(Clone, Debug)]
pub struct RegInfo {
    pub pid: u32,
    pub pgid: i32,
    pub owner: Owner,
}

pub fn registered() -> Vec<RegInfo> {
    match registry().lock() {
        Ok(v) => v
            .iter()
            .map(|e| RegInfo { pid: e.pid, pgid: e.pgid, owner: e.owner.clone() })
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ── configure / register ────────────────────────────────────────────────────

/// [`configure`] 的产物:证明「已配进程组 + 已注 marker」,故只能对被 configure 过的
/// `Command` 所 spawn 的 child 调 [`Configured::register`]。`#[must_use]` 提醒别丢弃
/// (丢弃 = 忘了登记 = 该子进程游离于进程树治理之外)。
#[must_use = "spawn 后请调用 .register(&child) 把子进程纳入登记表,否则它游离于进程树治理之外"]
pub struct Configured {
    owner: Owner,
}

/// 把 `cmd` 配成 spawn 进**自己的进程组**(Unix)并携带实例/属主 marker。**紧邻
/// `.spawn()` 前**调用,spawn 后立刻 [`Configured::register`]。
///
/// 自成进程组是**安全前提**:保证直接子进程的组 ≠ Weft 自身的组,于是 reap 按组
/// SIGKILL 时绝不会误杀 Weft 本体(codex 的同组亲儿也随该组一起收走)。
pub fn configure(cmd: &mut Command, owner: Owner) -> Configured {
    cmd.env(ENV_INSTANCE, instance_id());
    cmd.env(ENV_OWNER, owner.tag());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    Configured { owner }
}

/// 一个直接子进程的登记记录。与它的 `Child` **同寿**(存同一处),使登记项生命周期 =
/// 子进程生命周期。Drop = **只摘登记**(元数据,不回收);回收走 [`reap`](T2 接线)。
pub struct Registration {
    id: u64,
    pid: u32,
    pgid: i32,
    owner: Owner,
    instance: &'static str,
}

impl Registration {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn pgid(&self) -> i32 {
        self.pgid
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn instance(&self) -> &'static str {
        self.instance
    }
}

impl Configured {
    /// 把刚 spawn 的 child 记入本实例登记表。因为 [`configure`] 用 `process_group(0)`
    /// 让 child 成为自己进程组的组长,故 `pgid == child pid`。
    pub fn register(self, child: &Child) -> Registration {
        // 刚 spawn、尚未 wait 的 child 一定有 pid;None 仅在已收尸后出现,此处不该发生。
        // 防御性取 0(而非 unwrap/expect —— 生产路径禁 panic):pid=0 的记录惰性无害
        // (is_ours 永不命中、kill_group 对 pgid<=1 拒发)。
        let pid = child.id().unwrap_or(0);
        let pgid = pid as i32;
        let id = next_reg_id();
        if pid != 0 {
            if let Ok(mut v) = registry().lock() {
                v.push(Entry { id, pid, pgid, owner: self.owner.clone() });
            }
        }
        Registration { id, pid, pgid, owner: self.owner, instance: instance_id() }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // **只摘登记(元数据),不做回收。** 回收(整树 SIGKILL + wait)的唯一入口是
        // [`reap`],由 T2 接进 shutdown_and_reap/stop_quiet 等所有 teardown 路径。T1 不在
        // Drop 里隐式双写回收——职责边界:T1 交付原语 + 登记,T2 接线 teardown。故本记录须
        // 与它的 `Child` 同寿(存同一处),登记项生命周期 = 子进程生命周期。按 id 摘登记
        // (见 `Entry::id`),故即便 pid 被复用也不会误删活着的另一登记项。
        deregister(self.id);
    }
}

// ── reap / kill ─────────────────────────────────────────────────────────────

/// **wait 收尸原语(树感知)**:趁直接子进程还活着,快照它的**后代闭包**,对闭包里
/// **每个不同的进程组各发一次 SIGKILL**(带走 codex + 它隔离在独立组里的每个 MCP
/// server + 子孙),再 `await` 直接子进程把它收尸(不留僵尸),最后摘登记。杀进程走
/// `killpg` = fork-free。
pub async fn reap(child: &mut Child, reg: &Registration) {
    kill_subtree(reg.pid as i32, reg.pgid);
    let _ = child.wait().await;
    deregister(reg.id);
}

/// Whether [`reap_bounded`] completed both tree discovery/termination and the
/// direct-child wait before its supplied budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundedReapOutcome {
    Completed,
    Incomplete,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum DeadlineWork<T> {
    Complete(T),
    Incomplete,
}

/// Deadline-aware variant of [`reap`] for latency-sensitive callers such as
/// readiness probes. It preserves [`reap`]'s full tree behavior only when the
/// complete process-table discovery, every tree-group kill, and the root wait
/// fit within `budget`.
///
/// Process-table reads are synchronous platform syscalls/filesystem reads, so
/// discovery runs in `spawn_blocking`. The outer deadline lets the async caller
/// return even if one OS read stalls; the detached worker owns only numeric PID
/// facts and repeatedly observes the same deadline while enumerating records
/// and walking the descendant graph. It never touches `child` or `reg` after a
/// caller has returned. An incomplete path always kills the known root group,
/// but deliberately does not deregister the live root: its owner must retain
/// the registration until it explicitly tears down or reaps that child.
///
/// On platforms without a fork-free descendant snapshot, this reports
/// [`BoundedReapOutcome::Incomplete`] after the root-group fallback rather than
/// claiming tree completion.
pub async fn reap_bounded(
    child: &mut Child,
    reg: &Registration,
    budget: Duration,
) -> BoundedReapOutcome {
    let Some(deadline) = Instant::now().checked_add(budget) else {
        return bounded_reap_incomplete(reg);
    };
    if Instant::now() >= deadline {
        return bounded_reap_incomplete(reg);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let root_pid = reg.pid as i32;
        let root_pgid = reg.pgid;
        let discovery_deadline = deadline;
        let discovery = tokio::task::spawn_blocking(move || {
            discover_tree_groups_until(root_pid, root_pgid, discovery_deadline)
        });
        let tokio_deadline = tokio::time::Instant::from_std(deadline);
        let groups = match tokio::time::timeout_at(tokio_deadline, discovery).await {
            Ok(Ok(DeadlineWork::Complete(groups))) => groups,
            Ok(Ok(DeadlineWork::Incomplete)) | Ok(Err(_)) | Err(_) => {
                return bounded_reap_incomplete(reg);
            }
        };

        if !kill_tree_groups_until(groups, reg.pgid, deadline) {
            return bounded_reap_incomplete(reg);
        }
        if Instant::now() >= deadline {
            return bounded_reap_incomplete(reg);
        }
        let wait_deadline = tokio::time::Instant::from_std(deadline);
        match tokio::time::timeout_at(wait_deadline, child.wait()).await {
            Ok(Ok(_)) => {
                deregister(reg.id);
                // `timeout_at` chose the completed wait. The child is now
                // reaped, so do not reinterpret a later clock observation as
                // incomplete and signal the old root PGID after its owner has
                // exited. Actual deadline expiry remains the `Err(_)` arm.
                BoundedReapOutcome::Completed
            }
            Ok(Err(_)) | Err(_) => BoundedReapOutcome::Incomplete,
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = child;
        bounded_reap_incomplete(reg)
    }
}

fn bounded_reap_incomplete(reg: &Registration) -> BoundedReapOutcome {
    kill_group(reg.pgid);
    BoundedReapOutcome::Incomplete
}

/// Discover every process group in the still-living descendant tree. The
/// caller supplies an absolute deadline so both snapshot enumeration and the
/// graph walk stop before doing more unbounded per-process work.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn discover_tree_groups_until(
    root_pid: i32,
    root_pgid: i32,
    deadline: Instant,
) -> DeadlineWork<HashSet<i32>> {
    let snapshot = match snapshot_until(deadline) {
        DeadlineWork::Complete(snapshot) => snapshot,
        DeadlineWork::Incomplete => return DeadlineWork::Incomplete,
    };
    tree_groups_from_snapshot_until(root_pid, root_pgid, &snapshot, || {
        Instant::now() < deadline
    })
}

/// A deadline-aware snapshot of `(pid, ppid, pgid)`. Linux iterates `/proc`
/// lazily and checks before advancing to the next entry; macOS checks around
/// its one kernel PID-list syscall and then before every per-PID lookup. The
/// outer `spawn_blocking` deadline in [`reap_bounded`] protects callers from an
/// individual platform read which cannot itself be preempted.
#[cfg(target_os = "macos")]
fn snapshot_until(deadline: Instant) -> DeadlineWork<Vec<(i32, i32, i32)>> {
    if Instant::now() >= deadline {
        return DeadlineWork::Incomplete;
    }
    let pids = all_pids();
    snapshot_from_pids_until(pids.into_iter(), || Instant::now() < deadline, proc_ppid_pgid)
}

#[cfg(target_os = "linux")]
fn snapshot_until(deadline: Instant) -> DeadlineWork<Vec<(i32, i32, i32)>> {
    if Instant::now() >= deadline {
        return DeadlineWork::Incomplete;
    }
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return DeadlineWork::Incomplete,
    };
    let pids = entries.filter_map(|entry| {
        let entry = entry.ok()?;
        entry.file_name().to_str()?.parse::<i32>().ok()
    });
    snapshot_from_pids_until(pids, || Instant::now() < deadline, proc_ppid_pgid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn snapshot_from_pids_until<I, HasTime, ReadFacts>(
    mut pids: I,
    mut has_time: HasTime,
    mut read_facts: ReadFacts,
) -> DeadlineWork<Vec<(i32, i32, i32)>>
where
    I: Iterator<Item = i32>,
    HasTime: FnMut() -> bool,
    ReadFacts: FnMut(i32) -> Option<(i32, i32)>,
{
    let mut snapshot = Vec::new();
    loop {
        if !has_time() {
            return DeadlineWork::Incomplete;
        }
        let Some(pid) = pids.next() else {
            return DeadlineWork::Complete(snapshot);
        };
        if !has_time() {
            return DeadlineWork::Incomplete;
        }
        if let Some((ppid, pgid)) = read_facts(pid) {
            snapshot.push((pid, ppid, pgid));
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn tree_groups_from_snapshot_until<HasTime>(
    root_pid: i32,
    root_pgid: i32,
    snapshot: &[(i32, i32, i32)],
    mut has_time: HasTime,
) -> DeadlineWork<HashSet<i32>>
where
    HasTime: FnMut() -> bool,
{
    if !has_time() {
        return DeadlineWork::Incomplete;
    }
    let mut groups: HashSet<i32> = HashSet::new();
    groups.insert(root_pgid);
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut pgid_by: HashMap<i32, i32> = HashMap::new();
    let mut root_seen = false;
    for &(pid, ppid, pgid) in snapshot {
        if !has_time() {
            return DeadlineWork::Incomplete;
        }
        if pid == root_pid {
            root_seen = true;
        }
        children.entry(ppid).or_default().push(pid);
        pgid_by.insert(pid, pgid);
    }
    // A missing root means it may already have exited and reparented an escaped
    // child. Do not claim a complete tree reap from that stale snapshot.
    if !root_seen {
        return DeadlineWork::Incomplete;
    }

    let mut seen: HashSet<i32> = HashSet::new();
    seen.insert(root_pid);
    let mut stack = vec![root_pid];
    while let Some(current) = stack.pop() {
        if !has_time() {
            return DeadlineWork::Incomplete;
        }
        if let Some(kids) = children.get(&current) {
            for &child in kids {
                if !has_time() {
                    return DeadlineWork::Incomplete;
                }
                if seen.insert(child) {
                    if let Some(&pgid) = pgid_by.get(&child) {
                        groups.insert(pgid);
                    }
                    stack.push(child);
                }
            }
        }
    }
    DeadlineWork::Complete(groups)
}

/// Kill the root group first, then every discovered escaped descendant group
/// while the deadline remains. Returning `false` means callers must preserve
/// their root-group fallback; the root itself was already signalled.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn kill_tree_groups_until(groups: HashSet<i32>, root_pgid: i32, deadline: Instant) -> bool {
    kill_group(root_pgid);
    for pgid in groups {
        if pgid == root_pgid {
            continue;
        }
        if Instant::now() >= deadline {
            return false;
        }
        kill_group(pgid);
    }
    true
}

/// 把 `root_pid` 为根的整棵进程子树按「每个不同进程组」SIGKILL。`root_pgid` 是直接子
/// 进程自己的组(configure 保证 == root_pid、且 ≠ Weft 的组)。快照在调用时刻取,故
/// 需在 root 还活着(ppid 链完好)时调用。fork-free。
fn kill_subtree(root_pid: i32, root_pgid: i32) {
    let mut groups: HashSet<i32> = HashSet::new();
    groups.insert(root_pgid);
    // 一次快照建 children 映射与 pid→pgid 映射,再在内存里 BFS,避免边遍历边 syscall。
    let snap = snapshot();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut pgid_by: HashMap<i32, i32> = HashMap::new();
    for &(pid, ppid, pgid) in &snap {
        children.entry(ppid).or_default().push(pid);
        pgid_by.insert(pid, pgid);
    }
    let mut seen: HashSet<i32> = HashSet::new();
    seen.insert(root_pid);
    let mut stack = vec![root_pid];
    while let Some(cur) = stack.pop() {
        if let Some(kids) = children.get(&cur) {
            for &k in kids {
                if seen.insert(k) {
                    if let Some(&g) = pgid_by.get(&k) {
                        groups.insert(g);
                    }
                    stack.push(k);
                }
            }
        }
    }
    for g in groups {
        kill_group(g);
    }
}

/// 只 SIGKILL 一个进程组、不 await。fork-free。
pub fn kill_group(pgid: i32) {
    // 绝不给进程组 0(== 调用者自己的组 → 会杀掉 Weft 自身!)或 1(init)发信号。
    // 合法的受管子进程 pgid == 它自己的 pid(全新的组),永不等于这两者。
    if pgid <= 1 {
        return;
    }
    #[cfg(unix)]
    {
        // 再防一手:绝不杀本进程所在的组。
        if pgid == own_pgid() {
            return;
        }
        // SAFETY: killpg 是纯 syscall,无内存安全前置条件。
        unsafe {
            let _ = libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
    }
}

#[cfg(unix)]
fn own_pgid() -> i32 {
    // SAFETY: getpgrp 无参、无内存安全前置条件。
    unsafe { libc::getpgrp() }
}

/// A unique open-file identity inherited by one subprocess tree.
///
/// The parent keeps the descriptor `CLOEXEC`; [`attach`](Self::attach) clears
/// that bit only in the selected command's post-fork child. Consequently,
/// concurrent probes do not inherit one another's markers, while descendants
/// of the selected command retain the descriptor across `fork`, `exec`,
/// `setsid`, and PPID reparenting. Sweeping compares vnode `(device, inode)`
/// identity rather than a path, so it remains valid even after unlink.
///
/// A descendant that deliberately closes unknown file descriptors has opted
/// out of this ownership channel and cannot be recovered by this fallback.
pub struct InheritedProcessMarker {
    #[cfg(unix)]
    file: Option<std::fs::File>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    armed: bool,
}

impl InheritedProcessMarker {
    pub fn create(label: &str) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::MetadataExt;

            static NEXT_MARKER: AtomicU64 = AtomicU64::new(1);
            let safe_label: String = label
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                        character
                    } else {
                        '_'
                    }
                })
                .take(48)
                .collect();
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            for _ in 0..32 {
                let sequence = NEXT_MARKER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "weft-process-marker-{safe_label}-{}-{nanos}-{sequence}",
                    instance_id()
                ));
                let file = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                };
                let fd = file.as_raw_fd();
                // Keep the descriptor private to Weft unless a command's
                // pre-exec hook explicitly opts into this marker.
                // SAFETY: `fd` belongs to `file` and remains live here.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                if flags < 0 {
                    let error = std::io::Error::last_os_error();
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                if flags & libc::FD_CLOEXEC == 0 {
                    // SAFETY: setting descriptor flags on our own live fd has
                    // no additional memory-safety preconditions.
                    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
                        let error = std::io::Error::last_os_error();
                        let _ = std::fs::remove_file(&path);
                        return Err(error);
                    }
                }
                let metadata = match file.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = std::fs::remove_file(&path);
                        return Err(error);
                    }
                };
                // Ownership is the open vnode, not the directory entry. Unlink
                // immediately so another process cannot open the marker by
                // discovering its temporary name, and a crash leaves no file.
                if let Err(error) = std::fs::remove_file(&path) {
                    return Err(error);
                }
                return Ok(Self {
                    file: Some(file),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    armed: true,
                });
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique process marker",
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = label;
            Ok(Self {})
        }
    }

    pub fn attach(&self, command: &mut Command) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let Some(file) = self.file.as_ref() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "process marker is no longer armed",
                ));
            };
            let fd = file.as_raw_fd();
            // SAFETY: the closure runs after fork and before exec. It performs
            // only `fcntl` plus construction of an OS error on failure.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        {
            let _ = command;
        }
        Ok(())
    }

    /// Kill every live process that still owns this marker, then sweep each
    /// marked process group. The caller may repeat this until it returns zero.
    pub fn sweep(&self) -> usize {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            sweep_open_file_identity(OpenFileIdentity {
                device: self.device,
                inode: self.inode,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            0
        }
    }

    /// Perform the final ownership sweep and suppress the Drop fallback.
    /// Readiness runs this on a bounded blocking worker; disarming prevents a
    /// second full process-table scan when the marker then leaves scope.
    pub fn sweep_and_disarm(&mut self) -> usize {
        let killed = self.sweep();
        #[cfg(unix)]
        {
            self.armed = false;
        }
        killed
    }
}

impl Drop for InheritedProcessMarker {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if self.armed {
                self.armed = false;
                if let Some(file) = self.file.take() {
                    enqueue_open_file_cleanup(OpenFileCleanup {
                        // Keep the unlinked vnode allocated until the queued
                        // scan finishes. Closing the last descriptor here
                        // could let the inode be reused and make a delayed
                        // identity-only scan target an unrelated process.
                        _file: file,
                        identity: OpenFileIdentity {
                            device: self.device,
                            inode: self.inode,
                        },
                    });
                }
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy)]
struct OpenFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct OpenFileCleanup {
    _file: std::fs::File,
    identity: OpenFileIdentity,
}

/// Drop may run on a Tokio worker when a readiness future is cancelled. Queue
/// the fallback scan onto one dedicated process-cleanup thread instead of
/// synchronously enumerating every PID/file descriptor on the async executor.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn enqueue_open_file_cleanup(cleanup: OpenFileCleanup) {
    const MAX_QUEUED_MARKER_CLEANUPS: usize = 64;
    static SENDER: OnceLock<std::sync::mpsc::SyncSender<OpenFileCleanup>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        // The queue owns live file descriptors so delayed scans cannot match a
        // reused inode. Bound it to keep repeated cancellation from turning a
        // stalled platform scan into unbounded descriptor retention. When the
        // queue is saturated, evidence is already fail-closed; relinquishing
        // this best-effort fallback is safer than blocking a runtime worker or
        // retaining an unbounded number of live descriptors.
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<OpenFileCleanup>(MAX_QUEUED_MARKER_CLEANUPS);
        let _ = std::thread::Builder::new()
            .name("weft-process-marker-cleanup".to_string())
            .spawn(move || {
                for cleanup in receiver {
                    let _ = sweep_open_file_identity(cleanup.identity);
                }
            });
        sender
    });
    let _ = sender.try_send(cleanup);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sweep_open_file_identity(identity: OpenFileIdentity) -> usize {
    let own_pid = std::process::id() as i32;
    let marked: Vec<i32> = all_pids()
        .into_iter()
        .filter(|pid| {
            *pid != own_pid && process_has_open_file(*pid, identity.device, identity.inode)
        })
        .collect();
    let mut groups = HashSet::new();
    let mut killed = 0;
    for pid in marked {
        // Revalidate immediately before signalling to narrow the PID reuse
        // race between enumeration and kill.
        if !process_has_open_file(pid, identity.device, identity.inode) {
            continue;
        }
        if let Some((_, pgid)) = proc_ppid_pgid(pid) {
            groups.insert(pgid);
        }
        // SAFETY: signalling a numeric PID has no memory-safety precondition.
        // The marker recheck above establishes ownership.
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
        killed += 1;
    }
    for group in groups {
        kill_group(group);
    }
    killed
}

#[cfg(target_os = "linux")]
fn process_has_open_file(pid: i32, device: u64, inode: u64) -> bool {
    use std::os::unix::fs::MetadataExt;

    if pid <= 1 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        // `DirEntry::metadata` does not follow symlinks on this API. Proc fd
        // rows are symlinks, so use `fs::metadata` to inspect the open target.
        std::fs::metadata(entry.path())
            .map(|metadata| metadata.dev() == device && metadata.ino() == inode)
            .unwrap_or(false)
    })
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcessFileInfo {
    open_flags: u32,
    status: u32,
    offset: libc::off_t,
    file_type: i32,
    guard_flags: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfo {
    file: ProcessFileInfo,
    vnode: libc::vnode_info,
}

#[cfg(target_os = "macos")]
fn process_has_open_file(pid: i32, device: u64, inode: u64) -> bool {
    const PROC_PIDFDVNODEINFO: libc::c_int = 1;

    if pid <= 1 {
        return false;
    }
    let entry_size = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks libproc for the current byte requirement.
    let needed = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return false;
    }
    let capacity = (needed as usize)
        .saturating_add(entry_size.saturating_mul(32))
        .min(libc::c_int::MAX as usize);
    let mut entries = vec![libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    }; capacity / entry_size];
    // SAFETY: `entries` owns `capacity` writable bytes and libproc fills at
    // most the supplied byte count.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr() as *mut libc::c_void,
            capacity as libc::c_int,
        )
    };
    if filled <= 0 {
        return false;
    }
    entries.truncate((filled as usize / entry_size).min(entries.len()));
    entries.into_iter().any(|entry| {
        if entry.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            return false;
        }
        let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
        let info_size = std::mem::size_of::<VnodeFdInfo>() as libc::c_int;
        // SAFETY: `info` is a correctly sized writable C-layout buffer for
        // PROC_PIDFDVNODEINFO.
        let read = unsafe {
            libc::proc_pidfdinfo(
                pid,
                entry.proc_fd,
                PROC_PIDFDVNODEINFO,
                &mut info as *mut _ as *mut libc::c_void,
                info_size,
            )
        };
        read == info_size
            && u64::from(info.vnode.vi_stat.vst_dev) == device
            && info.vnode.vi_stat.vst_ino == inode
    })
}

// ── 口径:is_ours / count ────────────────────────────────────────────────────

/// **唯一口径。** 一个存活 OS 进程属于本实例 ⟺ 返回 `true`:沿 ppid 上溯能到达某个登记
/// 在册的直接子进程(即它是某直接子进程的后代或其本身)。[`count_instance_processes`]
/// 与 T2 的孤儿判定都**只**调本函数,故两者口径不可能漂移。
pub fn is_ours(pid: i32) -> bool {
    is_descendant_of_registered(pid, &registered_pids())
}

/// 上溯 `pid` 的祖先链,命中 `roots`(登记的直接子进程)中任一即属于本实例。
fn is_descendant_of_registered(pid: i32, roots: &HashSet<i32>) -> bool {
    if pid <= 1 || roots.is_empty() {
        return false;
    }
    let mut cur = pid;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if roots.contains(&cur) {
            return true;
        }
        match ppid_of(cur) {
            Some(p) if p > 1 => cur = p,
            _ => return false,
        }
    }
    false
}

/// 本实例当前存活的 OS 进程数(直接子进程 + 它们的后代,如 codex + 它拉起的 MCP
/// server)。语义 = `所有存活 pid 里 is_ours 为真的数量` —— 与孤儿判定同一谓词(硬不
/// 变量)。syscall 级、fork-free,逼近 ulimit 时仍可安全调用。
///
/// **这是「归因量」,不是「fork 压力量」。** 它只数**本 Weft 实例自己**的子树(reap/
/// 池化要压降的正是这个数),**不含**别的进程 / 非-Weft 进程 / 别的 Weft 实例。判定
/// 是否临近 `fork EAGAIN`(降级/admit)应看**按真实 UID 的全进程数 vs RLIMIT_NPROC**
/// —— 那是 `process_quota` 的职责(`publish_sample` 的 count 用 per-uid 总量),别把本
/// 函数塞进去当降级分子,否则会在真 EAGAIN 前低估压力。二者并列:本数用于「Weft 占了
/// 多少」的归因/UI,per-uid 用于安全网。
pub fn count_instance_processes() -> usize {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        instance_pids().len()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // 无 fork-free 全局枚举的平台(如 Windows):退回登记的直接子进程数(尽力而为,
        // 不含孙进程)。**注意**:此分支上 `is_ours` 恒 false(无枚举),故「计数口径==
        // 孤儿判定口径」的硬不变量只在有 fork-free 枚举的平台(macOS/Linux,即 release
        // 目标)成立;非目标平台仅保证编译 + 一个粗略的直接子进程计数。
        registered().len()
    }
}

/// 本实例当前存活的全部进程 pid(直接子 + 后代闭包)。与 [`count_instance_processes`]
/// 同一口径(都过 `is_ours`)。供 §6 UI 归因,以及 T2 §2「存活期周期快照 → 崩溃后下次
/// 启动清扫」的持久化输入。
///
/// perf(fast-follow,接 UI gauge 时再做):当前是 O(存活进程数 × 祖先深度)的**逐跳
/// syscall 上溯**(每 pid 走一遍 `is_ours`)。若成为每秒轮询的热点,改用 [`kill_subtree`]
/// 那样「一次 `snapshot()` 建 children 映射 + 从 roots 向下 BFS」的写法(更快且点时一致);
/// `instance_group_ids` 亦可折进同一次快照。目前无消费者,故先保「count==filter(is_ours)」
/// 的严格单源写法(不变量测试直接对比二者)。
pub fn instance_pids() -> Vec<i32> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let roots = registered_pids();
        if roots.is_empty() {
            return Vec::new();
        }
        all_pids()
            .into_iter()
            .filter(|&pid| is_descendant_of_registered(pid, &roots))
            .collect()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// 本实例子树里出现的**全部不同进程组 id**(直接子进程各自的组 + codex 隔离出的每个
/// MCP server 各自的组)。供 T2 崩溃兜底(§2):存活期周期快照这些 pgid 并持久化,下次
/// 启动对它们 `kill_group`(pgid 稳定、进程 reparent 到 init 后仍不变,故硬崩溃留下的孤儿
/// 也能按 pgid 收走 —— 这是绕过 codex「清洗环境」隔离、跨重启回收的唯一可行锚点)。
pub fn instance_group_ids() -> Vec<i32> {
    let mut groups: HashSet<i32> = HashSet::new();
    for pid in instance_pids() {
        if let Some((_, pgid)) = proc_ppid_pgid(pid) {
            groups.insert(pgid);
        }
    }
    groups.into_iter().collect()
}

// ── UI 归因(issue #112 资源仪表盘,只读)──────────────────────────────────────
//
// 下面几个函数只**读**既有登记表 / `instance_pids()`,不改动上面的口径、reap 或
// admission 逻辑;供只读资源面板展示「进程树从哪儿来」与「大概占多少内存」。

/// 一个 owner 分类的直接子进程计数,供仪表盘的「进程树」展示。数的是**登记表里
/// 的直接子进程**(session/lead_thread/curator/... 各开了几个受管进程),不含它们
/// 的后代——后代总量由 [`count_instance_processes`] 单独给出,两者在 UI 上并列
/// 展示(「共 N 个进程,来自 M 个会话 + ...」),不是同一层级、不重复计数。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerCount {
    pub kind: &'static str,
    pub count: u64,
}

/// 按 owner kind 分组统计登记表(见 [`OwnerCount`])。只保留非零分类,顺序固定为
/// [`OwnerKind::all`] 的声明顺序(不是 hash 顺序,也不是手写数组),让前端每次渲染
/// 的分类顺序不跳动,且新增 `OwnerKind` 变体时不会静默漏出这份分组。
pub fn instance_owner_counts() -> Vec<OwnerCount> {
    let regs = registered();
    OwnerKind::all()
        .into_iter()
        .filter_map(|kind| {
            let count = regs.iter().filter(|r| r.owner.kind == kind).count() as u64;
            (count > 0).then_some(OwnerCount { kind: kind.as_str(), count })
        })
        .collect()
}

/// 本实例 owned 子树(见 [`instance_pids`])**加上 Weft 自身**的常驻内存(RSS)合计,
/// 单位字节。逐 pid 走 fork-free 的平台 syscall(macOS `proc_pidinfo` / Linux
/// `/proc/<pid>/status`),单个 pid 读失败(已退出等)按 0 计入合计,不让整体求和因
/// 单点失败而报废——与 `count_instance_processes` 同样的「尽力而为」哲学。`None` 仅
/// 在平台没有 fork-free 枚举时出现(与 [`instance_pids`] 同一条件);真实测得「当前
/// 0 个 owned 子进程」时是 `Some(自身 RSS)`,不是 `None` 也不是 `Some(0)`——调用方
/// 不应把三者混为一谈。
///
/// 故意**不**把 `std::process::id()` 塞进 [`instance_pids`] 本身:那份列表还喂给
/// [`instance_group_ids`] 的 T2 崩溃兜底(下次启动按 pgid `kill_group`),把 Weft 自己
/// 的 pid/pgid 混进去是危险的口径污染;这里只在内存求和这一步单独加一项自身读数,
/// [`instance_pids`] 与 [`count_instance_processes`] 的既有口径(仅后代闭包)不变。
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn instance_memory_bytes() -> Option<u64> {
    let subtree: u64 = instance_pids().iter().filter_map(|&pid| proc_resident_bytes(pid)).sum();
    let own = proc_resident_bytes(std::process::id() as i32).unwrap_or(0);
    Some(subtree + own)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn instance_memory_bytes() -> Option<u64> {
    None
}

/// [`count_instance_processes`] + [`instance_memory_bytes`], from a **single**
/// [`instance_pids`] scan. Callers that want both numbers together (the resource
/// dashboard's poll tick is the first — and, per `instance_pids`'s own doc, exactly
/// the "every-second polling" case it warned would need this) would otherwise call
/// the two functions above back-to-back, each independently paying for
/// `instance_pids`'s full O(存活进程数 × 祖先深度) scan — doubling the per-tick
/// syscall volume for numbers that are supposed to describe the same instant. This
/// walks the pid list once and derives both from that one snapshot, which also
/// removes the TOCTOU gap between them (they're now guaranteed to be the same pid
/// set, not two scans microseconds apart).
///
/// `count_instance_processes` and `instance_memory_bytes` are left exactly as they
/// are for any caller that only needs one of the two numbers — this is an additional
/// combined path, not a replacement.
#[derive(Clone, Copy, Debug)]
pub struct InstanceUsage {
    /// Same value [`count_instance_processes`] would return.
    pub process_count: usize,
    /// Same value [`instance_memory_bytes`] would return.
    pub memory_bytes: Option<u64>,
}

pub fn instance_usage() -> InstanceUsage {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let pids = instance_pids();
        // process_count stays the subtree-only count ([`count_instance_processes`]'s
        // existing contract, and what `instance_group_ids`'s T2 crash-fallback pgid
        // sweep implicitly relies on `instance_pids` never including Weft's own pid).
        // memory_bytes additionally folds in Weft's own RSS — see
        // [`instance_memory_bytes`]'s doc for why that addition is memory-only.
        let subtree: u64 = pids.iter().filter_map(|&pid| proc_resident_bytes(pid)).sum();
        let own = proc_resident_bytes(std::process::id() as i32).unwrap_or(0);
        InstanceUsage { process_count: pids.len(), memory_bytes: Some(subtree + own) }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Mirrors count_instance_processes' non-macOS/Linux fallback branch.
        InstanceUsage { process_count: registered().len(), memory_bytes: None }
    }
}

// ── 平台相关:进程枚举(fork-free)────────────────────────────────────────────

/// `(ppid, pgid)`。macOS 走 `proc_pidinfo`,Linux 读 `/proc/<pid>/stat`,均 fork-free、
/// 同 uid 可读。进程已消失/不可读 → `None`。
#[cfg(target_os = "macos")]
fn proc_ppid_pgid(pid: i32) -> Option<(i32, i32)> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: 传入本地栈上 proc_bsdinfo 及其正确 size;成功时内核填满 sz 字节。
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        )
    };
    if n == sz {
        Some((info.pbi_ppid as i32, info.pbi_pgid as i32))
    } else {
        None
    }
}

/// This pid's resident memory (RSS), in bytes. `None` if the pid vanished
/// mid-read or the syscall failed; callers sum via `filter_map` so one bad pid
/// (already exited) doesn't zero out the whole tree's total — mirrors
/// `proc_ppid_pgid`'s same "success iff kernel filled the full struct" check.
#[cfg(target_os = "macos")]
fn proc_resident_bytes(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: 传入本地栈上 proc_taskinfo 及其正确 size;成功时内核填满 sz 字节
    // (与上面 proc_ppid_pgid 的 PROC_PIDTBSDINFO 调用同一套路)。
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        )
    };
    if n == sz {
        Some(info.pti_resident_size)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<i32> {
    // SAFETY: 先以 NULL 探当前 pid 个数,再按容量取全量;buffersize 实参是字节数,
    // 但**返回值是 pid 个数,不是字节数**——`proc_listallpids` 包装了底层按字节计的
    // `proc_listpids`,内部已经 `/ sizeof(int)` 过一次才返回(Apple 开源 Libc
    // libsyscall/wrappers/libproc/libproc.c:`numpids = proc_listpids(...); return
    // numpids / sizeof(int);`,probe 调用与实取调用同一套逻辑)。之前这里又拿返回值
    // 除了一次 `size_of::<i32>()`,相当于把一个已经是「个数」的值当「字节数」二次
    // 折半再折半——四条里丢三条,新起的高 pid agent 尤其容易被截掉;这里改成直接把
    // 返回值当 pid 个数用,只在 SIZE 实参上保留字节单位。
    unsafe {
        let need = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if need <= 0 {
            return Vec::new();
        }
        // 宽松扩容防两次调用间进程增长。
        let cap = (need as usize) + 1024;
        let mut buf = vec![0i32; cap];
        let got = libc::proc_listallpids(
            buf.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<i32>()) as libc::c_int,
        );
        if got <= 0 {
            return Vec::new();
        }
        let count = (got as usize).min(cap);
        buf.truncate(count);
        buf.retain(|&p| p > 0);
        buf
    }
}

#[cfg(target_os = "linux")]
fn proc_ppid_pgid(pid: i32) -> Option<(i32, i32)> {
    if pid <= 0 {
        return None;
    }
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid_pgid(&s)
}

/// 从 `/proc/<pid>/stat` 内容解析 `(ppid, pgrp)`。格式 `pid (comm) state ppid pgrp ...`,
/// comm 可含空格与 `)`(如 `(a) b)`),故从**最后一个** `)` 之后开始按空白切:state、
/// ppid、pgrp。抽成纯函数以便对刁钻 comm 做单元测试(不依赖 /proc,故 cfg 到 test 亦编)。
#[cfg(any(target_os = "linux", test))]
fn parse_stat_ppid_pgid(s: &str) -> Option<(i32, i32)> {
    let after = s.get(s.rfind(')')? + 1..)?;
    let mut it = after.split_whitespace();
    let _state = it.next()?;
    let ppid = it.next()?.parse::<i32>().ok()?;
    let pgrp = it.next()?.parse::<i32>().ok()?;
    Some((ppid, pgrp))
}

/// This pid's resident memory (RSS), in bytes, read from `/proc/<pid>/status`.
/// `None` if the pid vanished mid-read or the file is malformed.
#[cfg(target_os = "linux")]
fn proc_resident_bytes(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_vmrss_kb(&s).map(|kb| kb.saturating_mul(1024))
}

/// Parse the `VmRSS:  1234 kB` line out of `/proc/<pid>/status` content.
/// Extracted as a pure function (mirrors `parse_stat_ppid_pgid` above) so it's
/// unit-testable without a real `/proc`, and compiles on non-Linux hosts too.
#[cfg(any(target_os = "linux", test))]
fn parse_vmrss_kb(s: &str) -> Option<u64> {
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn all_pids() -> Vec<i32> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) {
                out.push(pid);
            }
        }
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_ppid_pgid(_pid: i32) -> Option<(i32, i32)> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn all_pids() -> Vec<i32> {
    Vec::new()
}

fn ppid_of(pid: i32) -> Option<i32> {
    proc_ppid_pgid(pid).map(|(ppid, _)| ppid)
}

#[cfg(test)]
fn pgid_of(pid: i32) -> Option<i32> {
    proc_ppid_pgid(pid).map(|(_, pgid)| pgid)
}

/// 全体存活进程的 `(pid, ppid, pgid)` 快照(fork-free)。
fn snapshot() -> Vec<(i32, i32, i32)> {
    all_pids()
        .into_iter()
        .filter_map(|pid| proc_ppid_pgid(pid).map(|(ppid, pgid)| (pid, ppid, pgid)))
        .collect()
}

// ── 测试(合成子进程,不依赖 codex)──────────────────────────────────────────

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    /// 这些测试共享同一个进程级静态登记表,并对「本实例存活进程数」下断言;cargo 默认
    /// 并行跑测试会让彼此的 spawn/reap 互相污染计数。用一把串行锁保证同一时刻只有一个
    /// 进程测试在登记表里有条目 → 计数与不变量确定可复现。poison 容错:某测试 panic 也
    /// 不连累其余(拿回 inner guard 继续)。
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn null_cmd(program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        cmd
    }

    fn descendants(root: i32) -> Vec<i32> {
        let snap = snapshot();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for &(pid, ppid, _) in &snap {
            children.entry(ppid).or_default().push(pid);
        }
        let mut out = vec![root];
        let mut stack = vec![root];
        let mut seen: HashSet<i32> = [root].into_iter().collect();
        while let Some(cur) = stack.pop() {
            if let Some(kids) = children.get(&cur) {
                for &k in kids {
                    if seen.insert(k) {
                        out.push(k);
                        stack.push(k);
                    }
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn configure_puts_child_in_its_own_process_group() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-a"));
        let mut child = cmd.spawn().expect("spawn sh");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(200)).await;
        // 组长的 pgid 等于它自己的 pid,且 ≠ 测试进程(Weft)的组。
        assert_eq!(
            pgid_of(reg.pid() as i32),
            Some(reg.pid() as i32),
            "configured child must lead its own process group"
        );
        assert_ne!(reg.pgid(), own_pgid(), "child's group must differ from ours");
        reap(&mut child, &reg).await;
    }

    #[tokio::test]
    async fn count_includes_descendants() {
        let _g = test_guard();
        // sh(登记的直接子进程)+ 后台 sleep + 前台 sleep,两个 sleep 是 sh 的后代。
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30 & sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-b"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            count_instance_processes() >= 3,
            "instance count must include sh + its 2 sleep descendants"
        );
        reap(&mut child, &reg).await;
    }

    #[tokio::test]
    async fn reap_kills_descendants_even_when_they_escape_into_their_own_group() {
        let _g = test_guard();
        // 复现 codex 的隔离:一个直接子进程(perl)fork 出一个**在自己独立进程组里**的
        // 亲儿(POSIX::setpgid),正如 codex 把每个 MCP server 隔离进独立组。亲儿仍是
        // perl 的 child(ppid 链完好)但 pgid 不同 —— 朴素的 killpg(perl 组) 会漏掉它,
        // 树感知的 reap 必须照样把它杀掉。
        let mut cmd = null_cmd("perl");
        cmd.arg("-MPOSIX").arg("-e").arg(
            "my $pid=fork(); if(!$pid){ POSIX::setpgid(0,0); exec('sleep','30') } sleep 30;",
        );
        let cfg = configure(&mut cmd, Owner::other("test-c"));
        let Ok(mut child) = cmd.spawn() else {
            eprintln!("perl unavailable — skipping isolation test");
            return;
        };
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 找到那个「逃进独立组」的亲儿。
        let grandkids: Vec<i32> = descendants(reg.pid() as i32)
            .into_iter()
            .filter(|&p| p != reg.pid() as i32)
            .collect();
        assert!(
            !grandkids.is_empty(),
            "perl should have forked a grandchild; got {grandkids:?}"
        );
        let gk = grandkids[0];
        assert_ne!(
            pgid_of(gk),
            Some(reg.pgid()),
            "grandchild must have escaped into its OWN group (mimicking codex's MCP isolation)"
        );
        assert!(
            is_ours(gk),
            "grandchild is ours by the descendant criterion despite its separate group"
        );
        // §2 兜底锚点:逃逸亲儿的独立组必须出现在 instance_group_ids 里,T2 才能存活期
        // 快照它、崩溃后按 pgid 清扫。
        let escaped_group = pgid_of(gk).expect("grandchild alive");
        assert!(
            instance_group_ids().contains(&escaped_group),
            "the escaped grandchild's own group must be captured for crash-fallback sweep"
        );

        reap(&mut child, &reg).await;
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(
            pgid_of(gk),
            None,
            "tree-aware reap must kill the escaped grandchild's group, not only the leader's"
        );
        assert!(
            !registered().iter().any(|r| r.pid == reg.pid()),
            "reap deregisters the child"
        );
    }

    #[tokio::test]
    async fn inherited_fd_marker_reaps_a_background_child_after_its_parent_exits() {
        let _g = test_guard();
        let marker = InheritedProcessMarker::create("proc-registry-test")
            .expect("create inherited process marker");
        let root = tempfile::tempdir().expect("marker fixture directory");
        let pid_file = root.path().join("background.pid");
        let mut command = null_cmd("sh");
        command
            .env("WEFT_PROC_REGISTRY_TEST_PID_FILE", &pid_file)
            .arg("-c")
            .arg("sleep 30 >/dev/null 2>&1 & printf '%s\\n' \"$!\" > \"$WEFT_PROC_REGISTRY_TEST_PID_FILE\"");
        marker
            .attach(&mut command)
            .expect("attach inherited process marker");
        let configured = configure(&mut command, Owner::other("marker-parent-exit"));
        let mut child = command.spawn().expect("spawn marker fixture");
        let registration = configured.register(&child);
        child.wait().await.expect("wait direct shell");

        let background_pid = std::fs::read_to_string(&pid_file)
            .expect("background child records pid")
            .trim()
            .parse::<i32>()
            .expect("numeric background pid");
        let mut marker_visible = false;
        for _ in 0..40 {
            if process_has_open_file(background_pid, marker.device, marker.inode) {
                marker_visible = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            marker_visible,
            "the inherited fd marker must remain visible on reparented pid {background_pid}"
        );

        let killed = marker.sweep();
        assert!(
            killed > 0,
            "the inherited fd must retain ownership after the shell is reparented away"
        );
        let mut still_alive = true;
        for _ in 0..40 {
            if proc_ppid_pgid(background_pid).is_none() {
                still_alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            let _ = marker.sweep();
        }
        assert!(!still_alive, "the marked background process must be gone");
        drop(registration);
    }

    #[tokio::test]
    async fn dropping_inherited_fd_marker_queues_reparented_child_cleanup() {
        let _g = test_guard();
        let marker = InheritedProcessMarker::create("proc-registry-drop-test")
            .expect("create Drop cleanup marker");
        let root = tempfile::tempdir().expect("Drop cleanup fixture directory");
        let pid_file = root.path().join("drop-background.pid");
        let mut command = null_cmd("sh");
        command
            .env("WEFT_PROC_REGISTRY_TEST_PID_FILE", &pid_file)
            .arg("-c")
            .arg("sleep 30 >/dev/null 2>&1 & printf '%s\\n' \"$!\" > \"$WEFT_PROC_REGISTRY_TEST_PID_FILE\"");
        marker
            .attach(&mut command)
            .expect("attach Drop cleanup marker");
        let configured = configure(&mut command, Owner::other("marker-drop-parent-exit"));
        let mut child = command.spawn().expect("spawn Drop cleanup fixture");
        let registration = configured.register(&child);
        child.wait().await.expect("wait Drop cleanup shell");

        let background_pid = std::fs::read_to_string(&pid_file)
            .expect("Drop cleanup child records pid")
            .trim()
            .parse::<i32>()
            .expect("numeric Drop cleanup pid");
        assert!(
            process_has_open_file(background_pid, marker.device, marker.inode),
            "the Drop fixture must inherit the ownership marker"
        );

        drop(marker);
        for _ in 0..80 {
            if proc_ppid_pgid(background_pid).is_none() {
                drop(registration);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Some((_, group)) = proc_ppid_pgid(background_pid) {
            kill_group(group);
        }
        drop(registration);
        panic!("Drop cleanup did not reap marked child {background_pid}");
    }

    #[test]
    fn deadline_aware_snapshot_and_tree_walk_stop_before_full_input() {
        let mut snapshot_checks = 0;
        let mut fact_reads = 0;
        let snapshot = snapshot_from_pids_until(
            (1..=100).collect::<Vec<_>>().into_iter(),
            || {
                snapshot_checks += 1;
                snapshot_checks < 4
            },
            |_| {
                fact_reads += 1;
                Some((0, 1))
            },
        );
        assert!(matches!(snapshot, DeadlineWork::Incomplete));
        assert_eq!(
            fact_reads, 1,
            "the deadline check must stop PID enumeration before every record is read"
        );

        let mut walk_checks = 0;
        let walk = tree_groups_from_snapshot_until(
            1,
            1,
            &[(1, 0, 1), (2, 1, 2), (3, 2, 3)],
            || {
                walk_checks += 1;
                walk_checks < 4
            },
        );
        assert!(matches!(walk, DeadlineWork::Incomplete));
    }

    #[test]
    fn deadline_aware_tree_walk_keeps_each_discovered_process_group() {
        let walk = tree_groups_from_snapshot_until(
            1,
            1,
            &[(1, 0, 1), (2, 1, 2), (3, 2, 3)],
            || true,
        );
        match walk {
            DeadlineWork::Complete(groups) => {
                assert!(groups.contains(&1));
                assert!(groups.contains(&2));
                assert!(groups.contains(&3));
            }
            DeadlineWork::Incomplete => {
                panic!("an unlimited synthetic snapshot must finish its tree walk")
            }
        }
    }

    #[tokio::test]
    async fn bounded_reap_zero_budget_kills_root_but_keeps_registration_until_owner_teardown() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let configured = configure(&mut cmd, Owner::other("bounded-reap-zero-budget"));
        let mut child = cmd.spawn().expect("spawn bounded reaper fixture");
        let registration = configured.register(&child);
        let pid = registration.pid();

        let outcome = reap_bounded(&mut child, &registration, Duration::ZERO).await;
        assert_eq!(outcome, BoundedReapOutcome::Incomplete);
        assert!(
            registered().iter().any(|entry| entry.pid == pid),
            "an incomplete reap must retain the live-child registration for its owner"
        );

        let waited = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        assert!(
            matches!(waited, Ok(Ok(_))),
            "the incomplete fallback must still terminate the known root group"
        );
        drop(registration);
    }

    #[tokio::test]
    async fn bounded_reap_reports_completed_when_its_child_wait_wins_the_deadline() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let configured = configure(&mut cmd, Owner::other("bounded-reap-completed"));
        let mut child = cmd.spawn().expect("spawn bounded completed fixture");
        let registration = configured.register(&child);
        let pid = registration.pid();

        let outcome = reap_bounded(&mut child, &registration, Duration::from_secs(1)).await;
        assert_eq!(outcome, BoundedReapOutcome::Completed);
        assert!(
            !registered().iter().any(|entry| entry.pid == pid),
            "a completed bounded reap must deregister its reaped root"
        );
    }

    #[tokio::test]
    async fn is_ours_tracks_descendant_criterion() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-d"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(is_ours(reg.pid() as i32), "registered child is ours");
        assert!(!is_ours(1), "init (pid 1) is never ours");
        // 我们自己(测试进程)不是自己所 spawn 的子进程的后代 → 不属于「本实例的子树」。
        assert!(
            !is_ours(std::process::id() as i32),
            "the test process itself is an ancestor, not a descendant, of registered children"
        );

        reap(&mut child, &reg).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!is_ours(reg.pid() as i32), "reaped child is no longer ours");
    }

    /// 不变量守卫:`count_instance_processes` 必须恒等于「所有存活 pid 里 is_ours 为真的
    /// 数量」。若日后有人把 count 换成别的枚举路径(登记表长度、按组求和等)与 `is_ours`
    /// 口径漂移,此断言破 —— 计数口径与孤儿判定口径分家。
    #[tokio::test]
    async fn count_is_exactly_filter_is_ours() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30 & sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-e"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let manual = all_pids().into_iter().filter(|&p| is_ours(p)).count();
        assert_eq!(
            manual,
            count_instance_processes(),
            "count_instance_processes must be exactly filter(is_ours) — the single criterion"
        );
        reap(&mut child, &reg).await;
    }

    /// Regression guard for the `proc_listallpids` return-value bug (Codex review,
    /// PR #131): the wrapper's return is a **pid count**, not a byte count — dividing
    /// it by `size_of::<i32>()` again (as the old code did) kept only ~1/4 of the true
    /// pid list. The test process's own pid is trivially alive for the entire
    /// duration of this call, so a correct full-table scan must always find it;
    /// under the old bug it would be silently dropped whenever it fell outside
    /// whatever quarter of the kernel's fill order survived the bogus truncation.
    #[test]
    fn all_pids_includes_the_test_process_itself() {
        let me = std::process::id() as i32;
        let pids = all_pids();
        assert!(
            pids.contains(&me),
            "a correct full-table scan must include the caller's own live pid; got {} pids, self ({me}) missing",
            pids.len()
        );
    }

    /// Regression guard (Codex review, PR #131): the memory total must include
    /// Weft's OWN resident memory, not only its owned subtree's — the subtree can
    /// genuinely be empty (no agent running right now), and reporting 0 B in that
    /// case contradicted the dashboard's "this app plus every agent it spawned"
    /// copy. Doesn't need `test_guard`: it only asserts a floor from Weft's own
    /// always-positive RSS, which the unconditional own-pid addend guarantees
    /// regardless of what other tests concurrently register.
    #[test]
    fn instance_memory_bytes_includes_self_even_with_empty_subtree() {
        let bytes = instance_memory_bytes().expect("macos/linux always sample RSS");
        assert!(bytes > 0, "must include Weft's own resident memory even with no owned subtree");
    }

    /// 最安全关键的守卫:`kill_group` **绝不**给进程组 0(调用者自己的组)、1(init)或
    /// Weft 本进程所在的组发信号。放一个「与 Weft 同组」的哨兵(不 configure→继承测试进程
    /// 的组),对这三个禁忌目标各发一次,哨兵必须存活。若守卫失效,这条会连测试进程一起
    /// 杀掉 —— 强信号。
    #[tokio::test]
    async fn kill_group_never_signals_weft_or_init_group() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30").kill_on_drop(true);
        // 故意不 configure:哨兵留在 Weft 自己的进程组里。
        let mut sentinel = cmd.spawn().expect("spawn sentinel");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let spid = sentinel.id().expect("sentinel pid") as i32;
        assert_eq!(
            pgid_of(spid),
            Some(own_pgid()),
            "sentinel must share Weft's own group so an unguarded kill_group WOULD hit it"
        );

        kill_group(0); // 0 == 调用者自己的组
        kill_group(1); // init
        kill_group(own_pgid()); // Weft 自己的组

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            sentinel.try_wait().ok().flatten().is_none(),
            "kill_group must NEVER signal group 0 / 1 / Weft's own group"
        );
        let _ = sentinel.kill().await;
    }

    /// `/proc/<pid>/stat` 解析必须从**最后一个** `)` 之后切字段,才能扛住 comm 里含空格与
    /// `)` 的进程名。纯字符串、不依赖 /proc,故可在 macOS 上也跑。
    #[test]
    fn parse_stat_handles_comm_with_parens_and_spaces() {
        // comm = "weird ) proc"(含空格 + 内嵌右括号)。
        assert_eq!(
            parse_stat_ppid_pgid("1234 (weird ) proc) S 999 7777 7777 0 -1 4194304"),
            Some((999, 7777))
        );
        assert_eq!(parse_stat_ppid_pgid("42 (bash) R 7 13 13 0"), Some((7, 13)));
        // 畸形 → None(不 panic)。
        assert_eq!(parse_stat_ppid_pgid("nonsense-no-paren"), None);
        assert_eq!(parse_stat_ppid_pgid("42 (x) S"), None); // 缺 ppid/pgrp
    }

    /// 未经 `reap` 直接 drop 一个 `Registration` 也必须摘登记(元数据),否则登记表会积累
    /// 死条目。
    #[tokio::test]
    async fn drop_without_reap_deregisters() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30").kill_on_drop(true);
        let cfg = configure(&mut cmd, Owner::other("test-drop"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        let pid = reg.pid();
        assert!(
            registered().iter().any(|r| r.pid == pid),
            "registered right after register()"
        );
        drop(reg); // drop WITHOUT reap
        assert!(
            !registered().iter().any(|r| r.pid == pid),
            "dropping the registration deregisters even without reap"
        );
        let _ = child.kill().await; // 无 reap 发生,直接杀掉哨兵子进程收尾
    }

    /// `/proc/<pid>/status` 的 `VmRSS:` 行解析:纯字符串、不依赖 /proc,可在 macOS
    /// 上也跑(与 `parse_stat_handles_comm_with_parens_and_spaces` 同一惯例)。
    #[test]
    fn parse_vmrss_kb_reads_the_vmrss_line() {
        let status = "Name:\tsh\nVmPeak:\t   10240 kB\nVmRSS:\t    2048 kB\nVmHWM:\t   3072 kB\n";
        assert_eq!(parse_vmrss_kb(status), Some(2048));
        // 缺 VmRSS 行 → None(不 panic)。
        assert_eq!(parse_vmrss_kb("Name:\tsh\nVmPeak:\t 10240 kB\n"), None);
        // 畸形数值 → None。
        assert_eq!(parse_vmrss_kb("VmRSS:\tnot-a-number kB\n"), None);
    }

    /// 回归哨兵(记录性,非穷尽性来源):`OwnerKind::all()` 必须覆盖全部变体、无重复、
    /// 顺序即声明顺序。自改用 `owner_kinds!` 宏后,「变体存在」与「出现在 `all()`
    /// 里」已经是同一次宏展开的同一件事,穷尽性由宏结构本身担保(见宏定义处文档),
    /// 不再靠这条测试撑住。这条测试现在防的是另一件事:有人重排/增删
    /// `owner_kinds!` 调用里的行时,没意识到顺序变化会影响
    /// `instance_owner_counts` 对前端的顺序承诺(分类顺序不跳动)——用一份显式的
    /// 期望列表把当前顺序钉住,变动时逼着改动的人在这里也确认一遍。
    #[test]
    fn owner_kind_all_covers_every_variant_in_declaration_order() {
        assert_eq!(
            OwnerKind::all(),
            vec![
                OwnerKind::GlobalAppServer,
                OwnerKind::Other,
            ]
        );
    }

    /// 仪表盘的内存读数必须反映真实 owned 子树:起一个子进程后,合计 RSS 应 > 0。
    #[tokio::test]
    async fn instance_memory_bytes_reflects_owned_subtree() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-mem"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let bytes = instance_memory_bytes().expect("macOS/Linux always yield Some");
        assert!(bytes > 0, "owned subtree should report nonzero RSS, got {bytes}");
        reap(&mut child, &reg).await;
    }

    /// `instance_usage` 是单扫描版的 `count_instance_processes` + `instance_memory_bytes`
    /// 组合——两条读数必须与分别调用两个独立函数完全一致,否则「只扫一次」的优化就
    /// 悄悄改了语义。
    #[tokio::test]
    async fn instance_usage_matches_the_two_separate_functions_it_replaces() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let cfg = configure(&mut cmd, Owner::other("test-usage"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let usage = instance_usage();
        assert_eq!(
            usage.process_count,
            count_instance_processes(),
            "instance_usage's count must match count_instance_processes"
        );
        assert!(usage.memory_bytes.unwrap_or(0) > 0, "owned subtree should report nonzero RSS");
        // 两条读数本就来自 instance_pids() 的分别两次调用,进程数在几百毫秒内几乎
        // 不会变化,故允许极小误差而非要求逐字节相等。
        let separate = instance_memory_bytes().expect("macOS/Linux always yield Some");
        let combined = usage.memory_bytes.expect("macOS/Linux always yield Some");
        let diff = separate.abs_diff(combined);
        assert!(
            diff <= separate / 10 + 1024,
            "combined-scan RSS ({combined}) should closely match the separate call ({separate})"
        );

        reap(&mut child, &reg).await;
    }

    /// `instance_owner_counts` 分组必须按登记的 owner kind 统计,且过滤掉计数为零
    /// 的分类(前端渲染只关心「实际存在」的分类)。
    #[tokio::test]
    async fn owner_counts_group_by_kind_and_skip_zero() {
        let _g = test_guard();
        let mut cmd = null_cmd("sh");
        cmd.arg("-c").arg("sleep 30");
        let cfg = configure(&mut cmd, Owner::other("codex-session"));
        let mut child = cmd.spawn().expect("spawn");
        let reg = cfg.register(&child);
        tokio::time::sleep(Duration::from_millis(150)).await;

        let counts = instance_owner_counts();
        assert!(
            counts.iter().any(|c| c.kind == "other" && c.count >= 1),
            "the registered child must show up under the other kind"
        );
        assert!(
            counts.iter().all(|c| c.count > 0),
            "zero-count owner kinds must not appear in the breakdown"
        );
        reap(&mut child, &reg).await;
    }
}
