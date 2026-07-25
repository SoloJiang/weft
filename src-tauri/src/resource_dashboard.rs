//! 本地资源仪表盘(issue #112)—— 只读聚合。
//!
//! 进程树安全网(#90 `process_quota` / #91 `proc_registry` / #92 `session_gate`)
//! 各自已经在采集「现在多满」所需的全部状态,本模块**不新增采样**,只把三者已有的
//! 只读读数拼成一个快照,供前端一个仪表盘统一展示:
//! - `quota`:系统级进程配额(#90 的 governor 快照,warn/degrade/recover 三态)。
//! - `instance_process_count` / `instance_memory_bytes` / `by_owner`:Weft 自身
//!   owned 进程子树的规模与构成(#91,`proc_registry` 的「UI 归因」只读函数)。
//! - `active_sessions` / `max_sessions`:并发会话闸门的占用(#92)。
//!
//! 只读边界:本模块不 reap、不降级、不改任何安全网的写路径——纯粹的 GET。

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDashboardSnapshot {
    /// 系统级进程配额状态(与 `process_quota_status` 命令同源,状态口径完全一致)。
    pub quota: crate::process_quota::ProcessQuotaSnapshot,
    /// Weft 本实例 owned 进程子树的存活进程总数(含后代,语义同
    /// `proc_registry::count_instance_processes`;实际由 `proc_registry::instance_usage`
    /// 单次扫描给出,与 `instance_memory_bytes` 共享同一份 pid 快照)。
    pub instance_process_count: u64,
    /// 该子树的常驻内存(RSS)合计,字节。`None` = 当前平台没有 fork-free 内存采样
    /// (非 macOS/Linux);前端应展示「不可用」而非当作 0。
    pub instance_memory_bytes: Option<u64>,
    /// 按 owner 分类的直接子进程计数(session/lead_thread/curator/... ),供「进程树
    /// 从哪儿来」的展示;只含非零分类。
    pub by_owner: Vec<crate::proc_registry::OwnerCount>,
    /// 并发会话闸门当前占用的槽位数。
    pub active_sessions: u64,
    /// 并发会话闸门的槽位上限(`WEFT_MAX_ACTIVE_SESSIONS`,默认见 `session_gate`)。
    pub max_sessions: u64,
}

#[tauri::command]
pub async fn resource_dashboard_snapshot(
    governor: tauri::State<'_, crate::process_quota::ProcessQuotaGovernor>,
) -> Result<ResourceDashboardSnapshot, String> {
    let (active_sessions, max_sessions) = crate::session_gate::active_session_slots();
    let quota = governor.snapshot();
    let by_owner = crate::proc_registry::instance_owner_counts();
    // Single-scan: count + memory both derive from one `instance_pids()` snapshot
    // instead of each independently re-walking the full process table (see
    // `proc_registry::instance_usage`'s doc — this poll tick is exactly the
    // per-second-polling case `instance_pids` warned would need it). The scan
    // itself enumerates every live pid and walks an ancestor chain per pid — real
    // syscall work that scales with total processes on the box, not just Weft's
    // own subtree — so it runs on a blocking-pool thread rather than inline on
    // this async command, which otherwise shares the WebView's IPC-handling task
    // with every other command (this page polls every 3s; a stall here would
    // stall unrelated commands issued while it runs).
    let usage = tokio::task::spawn_blocking(crate::proc_registry::instance_usage)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ResourceDashboardSnapshot {
        quota,
        instance_process_count: usage.process_count as u64,
        instance_memory_bytes: usage.memory_bytes,
        by_owner,
        active_sessions: active_sessions as u64,
        max_sessions: max_sessions as u64,
    })
}
