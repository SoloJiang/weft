//! 活跃会话软上限:限制同时活跃的 lead/worker 会话数,把 spawn 风暴挡在源头。
//!
//! 与 [`crate::process_quota`] 的总进程数硬阈值互补——后者是达标才拒绝的被动闸
//! (总进程逼近 RLIMIT_NPROC 时降级),本 gate 是主动限并发会话数(超限**排队**,
//! 不丢会话)。真正的进程放大在每个会话 fork 的 codex + 多线程 MCP 孙进程(weft
//! 不在那个 spawn 点、限不了单会话的孙进程),但每会话的放大是常数,故限「同时活跃
//! 会话数」= 间接给总进程数封顶。这把此前需要人肉「分批控载」(一次别派太多 worker,
//! 否则瞬间打满 RLIMIT_NPROC)的操作自动化了。
//!
//! permit 与会话的 agent 进程生命周期 RAII 绑定:acquire 到的槽存进
//! `EngineInner.child_permit`,和 `child_reg` 挨着,进程被 take/overwrite/stop 清掉
//! 时一并 drop=自动释放槽,无需显式 release 调用。

use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 默认活跃会话上限。实测约 9 个全速会话(各自 codex + 若干多线程 MCP server)就能
/// 吃满 4000 的 per-uid 进程配额,故默认给一个保守值;`WEFT_MAX_ACTIVE_SESSIONS`
/// 可覆盖(下限 1)。想放开并行度就调高,配额吃紧的机器调低。
const DEFAULT_MAX_ACTIVE_SESSIONS: usize = 8;

/// 解析活跃会话上限:env 覆盖 → 默认 → 下限 1(0/非法值不至于把 gate 卡死到 0)。
/// 抽成 `Option<&str>` 入参以便单测(镜像 `lib.rs` 的 `*_from_env` 惯例)。
fn max_active_sessions_from(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_ACTIVE_SESSIONS)
        .max(1)
}

fn max_active_sessions() -> usize {
    max_active_sessions_from(std::env::var("WEFT_MAX_ACTIVE_SESSIONS").ok().as_deref())
}

/// 进程全局单例 semaphore,启动时按上限初始化一次(OnceLock,同 proc_registry 的
/// registry() 惯例)。上限在首次取用时定格——env 在进程启动时读一次即可。
fn gate() -> &'static Arc<Semaphore> {
    static G: OnceLock<Arc<Semaphore>> = OnceLock::new();
    G.get_or_init(|| Arc::new(Semaphore::new(max_active_sessions())))
}

/// 会话 spawn 其 agent 进程前调用:拿一个活跃槽。已到上限时在此 `await` 排队,直到
/// 某个在跑的会话结束、drop 掉它的 permit 才轮到。返回的 permit 存进 `EngineInner`,
/// 随会话进程一起 drop 即释放槽。
///
/// 返回 `Option`:semaphore 是永不 close 的静态单例,`acquire_owned` 只在 close 时
/// 报错,故正常路径恒为 `Some`;万一 `None`(不可能发生的 closed 态),调用方直接放行
/// 不设限——安全降级,不 panic(生产路径禁 unwrap/expect)。
pub async fn acquire_session_slot() -> Option<OwnedSemaphorePermit> {
    gate().clone().acquire_owned().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_override() {
        assert_eq!(max_active_sessions_from(Some("3")), 3);
        assert_eq!(max_active_sessions_from(Some(" 12 ")), 12);
    }

    #[test]
    fn falls_back_to_default_on_absent_or_garbage() {
        assert_eq!(max_active_sessions_from(None), DEFAULT_MAX_ACTIVE_SESSIONS);
        assert_eq!(
            max_active_sessions_from(Some("not-a-number")),
            DEFAULT_MAX_ACTIVE_SESSIONS
        );
        assert_eq!(
            max_active_sessions_from(Some("")),
            DEFAULT_MAX_ACTIVE_SESSIONS
        );
    }

    #[test]
    fn floors_at_one_so_gate_never_deadlocks_at_zero() {
        assert_eq!(max_active_sessions_from(Some("0")), 1);
    }

    /// The gate's core contract: at most N permits out at once; the (N+1)th
    /// acquire blocks until one is dropped, then proceeds — i.e. queuing, not
    /// rejection. Verified on a local semaphore (the global one is env-sized).
    #[tokio::test]
    async fn nth_plus_one_queues_until_a_slot_frees() {
        let sem = Arc::new(Semaphore::new(2));
        let p1 = sem.clone().acquire_owned().await.expect("p1");
        let _p2 = sem.clone().acquire_owned().await.expect("p2");
        // Both slots taken → a third acquire must not resolve yet.
        assert!(sem.clone().try_acquire_owned().is_err());
        // Free one slot; now a waiter can proceed.
        drop(p1);
        assert!(sem.clone().try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn acquire_session_slot_yields_a_permit() {
        // Smoke: the real gate hands out at least one slot.
        let permit = acquire_session_slot().await;
        assert!(permit.is_some());
    }
}
