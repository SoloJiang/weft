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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::process::{Child, Command};

/// 每个受管子进程携带的实例标记 env。codex 会清洗它(见模块文档),故它**不是**主
/// 口径;对不清洗环境的 spawner(opencode/preview 等)及调试/未来跨重启仍有价值。
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

/// 逻辑属主 = 谁要的这个子进程,便于按属主整树收尸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerKind {
    /// 全局(app-scoped)codex app-server。
    GlobalAppServer,
    /// 某个 worker 会话。
    Session,
    /// 某个 lead thread 会话。
    LeadThread,
    /// 仓库图谱扫描(每仓库/关系一发,短命)。
    Curator,
    /// opencode server。
    Opencode,
    /// rewind 预览静态服务。
    Preview,
    /// 探测(版本/能力,短命)。
    Probe,
    /// 其它 / 测试。
    Other,
}

impl OwnerKind {
    fn as_str(self) -> &'static str {
        match self {
            OwnerKind::GlobalAppServer => "global_app_server",
            OwnerKind::Session => "session",
            OwnerKind::LeadThread => "lead_thread",
            OwnerKind::Curator => "curator",
            OwnerKind::Opencode => "opencode",
            OwnerKind::Preview => "preview",
            OwnerKind::Probe => "probe",
            OwnerKind::Other => "other",
        }
    }
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
    pub fn session(id: impl Into<String>) -> Owner {
        Owner::new(OwnerKind::Session, id)
    }
    pub fn lead_thread(id: impl Into<String>) -> Owner {
        Owner::new(OwnerKind::LeadThread, id)
    }
    pub fn curator() -> Owner {
        Owner::new(OwnerKind::Curator, "")
    }
    pub fn opencode(id: impl Into<String>) -> Owner {
        Owner::new(OwnerKind::Opencode, id)
    }
    pub fn preview(id: impl Into<String>) -> Owner {
        Owner::new(OwnerKind::Preview, id)
    }
    pub fn probe() -> Owner {
        Owner::new(OwnerKind::Probe, "")
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

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<i32> {
    // SAFETY: 先以 NULL 探需要的字节数,再按容量取全量;返回值为写入字节数。
    unsafe {
        let need_bytes = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if need_bytes <= 0 {
            return Vec::new();
        }
        // proc_listallpids 返回「字节数」;pid 为 i32。宽松扩容防两次调用间进程增长。
        let cap = (need_bytes as usize) / std::mem::size_of::<i32>() + 1024;
        let mut buf = vec![0i32; cap];
        let got_bytes = libc::proc_listallpids(
            buf.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<i32>()) as libc::c_int,
        );
        if got_bytes <= 0 {
            return Vec::new();
        }
        let count = ((got_bytes as usize) / std::mem::size_of::<i32>()).min(cap);
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
}
