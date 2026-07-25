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
//!
//! RAII 只保证「drop 即释放」,不保证「该 drop 时真的 drop 了」——`gate()` 是进程级
//! `OnceLock` 单例,漏掉任何一处清 `child` 却不清 `child_permit` 的地方,那个槽就一直
//! 占到应用重启为止(而「点 Stop」是高频操作)。所以清点是**穷举**的,清单见
//! `EngineInner.child_permit` 的字段文档;端到端回归见 engine.rs 的
//! `stop_quiet_releases_the_session_gate_slot`(本模块自己的测试只覆盖信号量原语)。

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

/// The slot ceiling `gate()` was actually sized with. `max_active_sessions()` is a
/// pure function of an env var that's fixed for the process lifetime, so re-reading
/// it here is deterministic and always agrees with what `gate()` used — but memoizing
/// it too (mirroring `gate()`'s own `OnceLock`) makes that agreement structural
/// rather than "trust the env doesn't change", which is what a read-only dashboard
/// snapshot needs to never show a `max` that could disagree with the live semaphore.
fn configured_max() -> usize {
    static M: OnceLock<usize> = OnceLock::new();
    *M.get_or_init(max_active_sessions)
}

/// Pure bookkeeping: given the semaphore's current `available` permits out of
/// `max`, how many are held right now. Extracted so the arithmetic is
/// unit-testable in isolation, independent of the process-wide `gate()`.
fn slots_from_available(available: usize, max: usize) -> (usize, usize) {
    (max.saturating_sub(available), max)
}

/// Read-only snapshot of the gate for the resource dashboard (issue #112):
/// `(active, max)` — how many of the configured slots are held right now vs the
/// ceiling. Pure read via `Semaphore::available_permits()`; never acquires or
/// releases a permit, so calling it cannot perturb queuing or admission.
pub fn active_session_slots() -> (usize, usize) {
    slots_from_available(gate().available_permits(), configured_max())
}

/// Serializes the tests that assert an EXACT before/after slot count on the
/// process-wide `gate()`. A leak is only observable as a delta ("the slot came
/// back"), and `gate()` is one static shared by every test in this binary, so
/// any test measuring that delta has to be the only one holding or acquiring a
/// slot while it does. Tokio mutexes don't poison, so a panicking test releases
/// this without cascading into the others.
#[cfg(test)]
pub(crate) fn gate_test_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
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
        // Holds a slot on the shared static gate → serialize against the tests
        // that measure exact slot deltas (see `gate_test_lock`).
        let _serialized = gate_test_lock().lock().await;
        // Smoke: the real gate hands out at least one slot.
        let permit = acquire_session_slot().await;
        assert!(permit.is_some());
    }

    /// The leak's shape, on a local semaphore: a permit parked in a long-lived
    /// struct field (what `EngineInner.child_permit` is) keeps its slot until
    /// that field is cleared. Overwriting the *neighbouring* field — the
    /// engine's `child` — releases nothing, which is precisely why every
    /// `child = None` site has to clear `child_permit` too.
    #[tokio::test]
    async fn a_parked_permit_holds_its_slot_until_the_field_is_cleared() {
        let sem = Arc::new(Semaphore::new(1));
        struct Session {
            child: Option<&'static str>,
            permit: Option<OwnedSemaphorePermit>,
        }
        let mut s = Session {
            child: Some("agent"),
            permit: sem.clone().acquire_owned().await.ok(),
        };
        assert_eq!(sem.available_permits(), 0);
        s.child = None; // "stop" that only clears the child …
        assert_eq!(sem.available_permits(), 0, "… leaks the slot");
        s.permit = None; // … and the same stop done right
        assert_eq!(sem.available_permits(), 1);
    }

    #[test]
    fn slots_from_available_computes_used_as_max_minus_available() {
        assert_eq!(slots_from_available(8, 8), (0, 8));
        assert_eq!(slots_from_available(5, 8), (3, 8));
        assert_eq!(slots_from_available(0, 8), (8, 8));
    }

    /// Smoke test against the REAL process-wide gate. Other tests in this binary
    /// (this module's own `acquire_session_slot_yields_a_permit`, plus any engine
    /// test exercising a real session spawn) can concurrently hold permits on the
    /// same static `gate()`, so this only asserts structural invariants that must
    /// hold no matter what else is mid-flight — never an exact before/after delta,
    /// which would be flaky under that shared global state.
    #[tokio::test]
    async fn active_session_slots_reports_within_bounds() {
        let (used, max) = active_session_slots();
        assert!(max >= 1, "the gate always has at least one slot");
        assert!(used <= max, "used ({used}) must never exceed max ({max})");
    }
}
