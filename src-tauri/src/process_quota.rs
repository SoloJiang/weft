//! Process-quota pressure state and admission control.
//!
//! The process registry owns the authoritative instance-process count. This
//! module only turns `(count, RLIMIT_NPROC)` samples into a hysteretic state,
//! publishes the snapshot, and rejects new work while degraded.

use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

pub const EVENT: &str = "process-quota://changed";
pub const DEGRADED_ERROR_CODE: &str = "process_quota_degraded";

pub const WARNING_PERCENT: u64 = 80;
pub const DEGRADED_PERCENT: u64 = 90;
pub const RECOVERY_PERCENT: u64 = 70;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessQuotaStatus {
    #[default]
    Normal,
    Warning,
    Degraded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessQuotaSnapshot {
    pub status: ProcessQuotaStatus,
    pub process_count: u64,
    pub process_limit: Option<u64>,
    pub usage_percent: Option<u64>,
    pub warning_percent: u64,
    pub degraded_percent: u64,
    pub recovery_percent: u64,
    /// Monotonic for this app run; changes only when `status` changes.
    pub transition_seq: u64,
}

impl Default for ProcessQuotaSnapshot {
    fn default() -> Self {
        Self {
            status: ProcessQuotaStatus::Normal,
            process_count: 0,
            process_limit: None,
            usage_percent: None,
            warning_percent: WARNING_PERCENT,
            degraded_percent: DEGRADED_PERCENT,
            recovery_percent: RECOVERY_PERCENT,
            transition_seq: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessQuotaUpdate {
    pub snapshot: ProcessQuotaSnapshot,
    pub status_changed: bool,
}

#[derive(Default)]
pub struct ProcessQuotaGovernor {
    snapshot: Mutex<ProcessQuotaSnapshot>,
}

impl ProcessQuotaGovernor {
    pub fn snapshot(&self) -> ProcessQuotaSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Apply one authoritative process-count sample.
    ///
    /// An absent/unlimited process limit cannot yield a meaningful percentage,
    /// so it fails open and clears a previous pressure state. The monitor still
    /// carries the count in the snapshot for diagnostics.
    pub fn observe(&self, process_count: u64, process_limit: Option<u64>) -> ProcessQuotaUpdate {
        let usable_limit = process_limit.filter(|limit| *limit > 0);
        let usage_percent = usable_limit.map(|limit| percent(process_count, limit));

        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        let next_status = next_status(current.status, process_count, usable_limit);
        let status_changed = next_status != current.status;
        let transition_seq = if status_changed {
            current.transition_seq.saturating_add(1)
        } else {
            current.transition_seq
        };

        *current = ProcessQuotaSnapshot {
            status: next_status,
            process_count,
            process_limit: usable_limit,
            usage_percent,
            warning_percent: WARNING_PERCENT,
            degraded_percent: DEGRADED_PERCENT,
            recovery_percent: RECOVERY_PERCENT,
            transition_seq,
        };

        ProcessQuotaUpdate {
            snapshot: current.clone(),
            status_changed,
        }
    }

    pub fn check_admission(&self) -> anyhow::Result<()> {
        if self.snapshot().status == ProcessQuotaStatus::Degraded {
            anyhow::bail!(DEGRADED_ERROR_CODE);
        }
        Ok(())
    }
}

fn next_status(
    current: ProcessQuotaStatus,
    process_count: u64,
    process_limit: Option<u64>,
) -> ProcessQuotaStatus {
    let Some(limit) = process_limit else {
        return ProcessQuotaStatus::Normal;
    };

    match current {
        ProcessQuotaStatus::Normal => {
            if ratio_at_least(process_count, limit, DEGRADED_PERCENT) {
                ProcessQuotaStatus::Degraded
            } else if ratio_at_least(process_count, limit, WARNING_PERCENT) {
                ProcessQuotaStatus::Warning
            } else {
                ProcessQuotaStatus::Normal
            }
        }
        ProcessQuotaStatus::Warning => {
            if ratio_at_least(process_count, limit, DEGRADED_PERCENT) {
                ProcessQuotaStatus::Degraded
            } else if ratio_at_most(process_count, limit, RECOVERY_PERCENT) {
                ProcessQuotaStatus::Normal
            } else {
                ProcessQuotaStatus::Warning
            }
        }
        ProcessQuotaStatus::Degraded => {
            if ratio_at_most(process_count, limit, RECOVERY_PERCENT) {
                ProcessQuotaStatus::Normal
            } else {
                ProcessQuotaStatus::Degraded
            }
        }
    }
}

fn ratio_at_least(count: u64, limit: u64, threshold: u64) -> bool {
    u128::from(count) * 100 >= u128::from(limit) * u128::from(threshold)
}

fn ratio_at_most(count: u64, limit: u64, threshold: u64) -> bool {
    u128::from(count) * 100 <= u128::from(limit) * u128::from(threshold)
}

fn percent(count: u64, limit: u64) -> u64 {
    let value = (u128::from(count) * 100) / u128::from(limit);
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The current per-user soft process limit. `None` means unsupported,
/// unavailable, zero, or unlimited; callers must not infer pressure from it.
#[cfg(unix)]
pub fn soft_process_limit() -> Option<u64> {
    let mut raw = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `raw` points to writable storage for exactly one `rlimit`, and we
    // only assume initialization when libc reports success.
    if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, raw.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: guarded by the successful `getrlimit` result above.
    let limit = unsafe { raw.assume_init() }.rlim_cur;
    if limit == 0 || limit == libc::RLIM_INFINITY {
        return None;
    }
    Some(limit as u64)
}

#[cfg(not(unix))]
pub fn soft_process_limit() -> Option<u64> {
    None
}

/// Live usage of the current real user against `RLIMIT_NPROC`, in that limit's
/// platform accounting unit — PROCESSES on macOS (`kern.maxprocperuid`), TASKS
/// (threads) on Linux (RLIMIT_NPROC counts every `clone()`). Feeding the unit
/// that MATCHES the limit is what lets the ratio predict a `fork` /
/// `pthread_create` EAGAIN; mixing units (e.g. processes vs a thread limit) would
/// undercount. Paired with [`soft_process_limit`] as the `(count, limit)` sample.
/// `None` = unsupported platform or a failed probe; callers fail open.
pub fn sample_process_count() -> Option<u64> {
    count_current_user_processes()
}

#[cfg(target_os = "macos")]
fn count_current_user_processes() -> Option<u64> {
    // `<sys/proc_info.h>` filter selectors (stable ABI); the `libc` crate
    // exposes `proc_listpids` but none of these constants. `PROC_RUID_ONLY`
    // filters by real uid, matching `getuid` and the RLIMIT_NPROC accounting.
    const PROC_ALL_PIDS: u32 = 1;
    const PROC_RUID_ONLY: u32 = 5;
    let stride = std::mem::size_of::<libc::c_int>();
    if stride == 0 {
        return None;
    }
    let uid = unsafe { libc::getuid() };
    // A null-buffer query returns an UPPER BOUND as a BYTE size: all pids
    // (total pid count x size_of::<c_int>()), because the uid filter is only
    // applied while filling a real buffer. Size for that worst case (dividing the
    // bytes by the stride), then read the ACTUAL filled byte length from the
    // filtered call.
    // SAFETY: the null-buffer / zero-size form is the documented "how big?"
    // query; it writes nothing and only reads our scalar arguments.
    let upper = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if upper <= 0 {
        return None;
    }
    // Slack for growth between the two calls; extra slots stay zero and are
    // filtered out below (`proc_listpids` writes at most `buffersize` bytes).
    let cap = (upper as usize / stride).saturating_add(32);
    let mut pids = vec![0 as libc::c_int; cap];
    let buf_bytes = i32::try_from(cap.saturating_mul(stride)).unwrap_or(i32::MAX);
    // SAFETY: `pids` owns `cap * stride` writable bytes; `proc_listpids` writes at
    // most `buf_bytes` of them and returns the number of bytes actually written.
    let written = unsafe {
        libc::proc_listpids(
            PROC_RUID_ONLY,
            uid,
            pids.as_mut_ptr() as *mut libc::c_void,
            buf_bytes,
        )
    };
    if written <= 0 {
        return None;
    }
    // Count real pids only: the kernel can leave zero-padded holes in the tail.
    let filled = (written as usize / stride).min(cap);
    Some(pids[..filled].iter().filter(|&&pid| pid != 0).count() as u64)
}

/// Sum the live threads (`<proc_root>/<pid>/task/*`) of every process under
/// `proc_root` owned by `uid`, following the Linux `/proc` layout. Linux
/// `RLIMIT_NPROC` accounts THREADS, not processes, so this — not a top-level
/// `/proc/<pid>` count — is the numerator that shares a unit with the limit.
/// Extracted (and parameterized on `proc_root`) so the counting logic compiles
/// and is unit-tested on every platform, not only where a real `/proc` exists.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn count_tasks_under(proc_root: &std::path::Path, uid: u32) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut threads: u64 = 0;
    for entry in std::fs::read_dir(proc_root).ok()?.flatten() {
        let name = entry.file_name();
        let is_pid = name
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        // `/proc/<pid>` is owned by the process's EFFECTIVE uid (kernel
        // `task_dump_owner`) — a small skew vs RLIMIT_NPROC's real-uid accounting
        // (a setuid child of ours undercounts; a process that adopted our euid
        // overcounts), negligible next to getting the process-vs-thread unit right.
        if entry.metadata().map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        // Count this process's threads; if `task/` is unreadable (it exited
        // mid-scan) still count it as at least one task.
        match std::fs::read_dir(entry.path().join("task")) {
            Ok(tasks) => {
                threads = threads.saturating_add(tasks.filter(|e| e.is_ok()).count() as u64)
            }
            Err(_) => threads = threads.saturating_add(1),
        }
    }
    Some(threads)
}

#[cfg(target_os = "linux")]
fn count_current_user_processes() -> Option<u64> {
    // Linux `RLIMIT_NPROC` limits TASKS (threads) for the real uid — it is
    // enforced in `copy_process()` on every `clone()`, including `CLONE_THREAD`.
    // A node/rust MCP server is one pid but many threads, so counting the
    // top-level `/proc/<pid>` process dirs would undercount the real fork pressure
    // by that multiplier and let the governor stay `Normal` right up to a real
    // `fork`/`pthread_create` EAGAIN. Count tasks to share the limit's unit.
    count_tasks_under(std::path::Path::new("/proc"), unsafe { libc::getuid() })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn count_current_user_processes() -> Option<u64> {
    None
}

pub fn admit_new_work(app: &AppHandle) -> anyhow::Result<()> {
    if let Some(governor) = app.try_state::<ProcessQuotaGovernor>() {
        governor.check_admission()?;
    }
    Ok(())
}

/// Store one sample and notify the frontend while pressure is visible or the
/// state just changed. Repeated normal samples stay silent.
pub fn publish_sample(app: &AppHandle, process_count: u64, process_limit: Option<u64>) {
    let Some(governor) = app.try_state::<ProcessQuotaGovernor>() else {
        return;
    };
    let update = governor.observe(process_count, process_limit);
    if update.status_changed || update.snapshot.status != ProcessQuotaStatus::Normal {
        let _ = app.emit(EVENT, update.snapshot);
    }
}

/// How often the sampler feeds the governor. Small enough that a burst of
/// session spawns cannot cross the 10% gap between the degraded threshold and
/// the hard limit before we observe it; the probe is cheap enough to run this
/// often at idle. Deliberately kept off the hot send path — admission reads the
/// last published sample and tolerates bounded within-interval overshoot.
const MONITOR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Sample process pressure into the governor for the life of the app. Same
/// managed-state + background-loop shape as `power::spawn_sweep`, but samples
/// BEFORE the first sleep (spawn_sweep sleeps first) so the launch snapshot is
/// real, then holds a fixed cadence. The probe (a `/proc` scan on Linux,
/// `proc_listpids` on macOS) is a blocking syscall whose cost grows with process
/// count — largest in the near-limit regime this guards — so it runs on a
/// blocking thread, never the async worker. A failed probe skips the tick rather
/// than forcing a fake `0`, which would flap a spurious "recovered" then
/// re-degrade. On platforms without a sampler the probe is always `None`, so the
/// governor stays `Normal` and admission stays open (fail-open).
pub fn spawn_monitor(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let probe =
                tokio::task::spawn_blocking(|| (sample_process_count(), soft_process_limit()))
                    .await;
            if let Ok((Some(count), limit)) = probe {
                publish_sample(&app, count, limit);
            }
            tokio::time::sleep(MONITOR_INTERVAL).await;
        }
    });
}

#[tauri::command]
pub fn process_quota_status(
    governor: State<'_, ProcessQuotaGovernor>,
) -> ProcessQuotaSnapshot {
    governor.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(governor: &ProcessQuotaGovernor, count: u64) -> ProcessQuotaSnapshot {
        governor.observe(count, Some(100)).snapshot
    }

    #[test]
    fn warning_boundary_and_recovery_are_hysteretic() {
        let governor = ProcessQuotaGovernor::default();
        assert_eq!(sample(&governor, 79).status, ProcessQuotaStatus::Normal);
        assert_eq!(sample(&governor, 80).status, ProcessQuotaStatus::Warning);
        assert_eq!(sample(&governor, 79).status, ProcessQuotaStatus::Warning);
        assert_eq!(sample(&governor, 71).status, ProcessQuotaStatus::Warning);
        assert_eq!(sample(&governor, 70).status, ProcessQuotaStatus::Normal);
    }

    #[test]
    fn degraded_boundary_stays_closed_until_recovery() {
        let governor = ProcessQuotaGovernor::default();
        assert_eq!(sample(&governor, 90).status, ProcessQuotaStatus::Degraded);
        assert!(governor.check_admission().is_err());
        assert_eq!(sample(&governor, 89).status, ProcessQuotaStatus::Degraded);
        assert_eq!(sample(&governor, 71).status, ProcessQuotaStatus::Degraded);
        assert_eq!(sample(&governor, 70).status, ProcessQuotaStatus::Normal);
        assert!(governor.check_admission().is_ok());
    }

    #[test]
    fn warning_can_escalate_without_recovering_first() {
        let governor = ProcessQuotaGovernor::default();
        assert_eq!(sample(&governor, 80).status, ProcessQuotaStatus::Warning);
        assert_eq!(sample(&governor, 90).status, ProcessQuotaStatus::Degraded);
    }

    #[test]
    fn unlimited_limit_fails_open_and_recovers() {
        let governor = ProcessQuotaGovernor::default();
        assert_eq!(sample(&governor, 95).status, ProcessQuotaStatus::Degraded);
        let snapshot = governor.observe(200, None).snapshot;
        assert_eq!(snapshot.status, ProcessQuotaStatus::Normal);
        assert_eq!(snapshot.process_limit, None);
        assert_eq!(snapshot.usage_percent, None);
    }

    #[test]
    fn transition_sequence_changes_only_with_status() {
        let governor = ProcessQuotaGovernor::default();
        assert_eq!(sample(&governor, 10).transition_seq, 0);
        assert_eq!(sample(&governor, 80).transition_seq, 1);
        assert_eq!(sample(&governor, 85).transition_seq, 1);
        assert_eq!(sample(&governor, 90).transition_seq, 2);
        assert_eq!(sample(&governor, 95).transition_seq, 2);
        assert_eq!(sample(&governor, 70).transition_seq, 3);
    }

    #[test]
    fn percentage_handles_over_limit_without_overflow() {
        let governor = ProcessQuotaGovernor::default();
        let snapshot = governor.observe(150, Some(100)).snapshot;
        assert_eq!(snapshot.usage_percent, Some(150));
        let snapshot = governor.observe(u64::MAX, Some(1)).snapshot;
        assert_eq!(snapshot.usage_percent, Some(u64::MAX));
    }

    /// The live probe must at minimum see the running test binary itself, so a
    /// supported platform never reports a nonsensical zero. Bounds the sampler
    /// that feeds `publish_sample`; the state machine above is exercised
    /// separately via direct `observe` calls.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn sampler_counts_at_least_this_process() {
        let count = sample_process_count().expect("supported platform yields a count");
        assert!(count >= 1, "expected >= 1 process for the current user, got {count}");
    }

    /// Exercises the Linux task-counting logic on any unix (incl. this macOS dev
    /// host, where the real `/proc` sampler is `proc_listpids`) via a synthetic
    /// `/proc` tree: threads are summed from `<pid>/task/*`, a missing `task/`
    /// falls back to one, non-numeric entries are ignored, and a foreign uid
    /// matches nothing.
    #[cfg(unix)]
    #[test]
    fn count_tasks_under_sums_owned_threads() {
        use std::fs;
        let uid = unsafe { libc::getuid() };
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // pid 100 with 3 threads; pid 200 with 1 thread.
        for tid in ["100", "101", "102"] {
            fs::create_dir_all(root.join("100").join("task").join(tid)).expect("mk task");
        }
        fs::create_dir_all(root.join("200").join("task").join("200")).expect("mk task");
        // pid 300 has no task/ dir -> the fallback still counts it as one task.
        fs::create_dir_all(root.join("300")).expect("mk pid");
        // A non-numeric entry (like real /proc's `bus`, `cpuinfo`) is ignored.
        fs::create_dir_all(root.join("bus")).expect("mk non-pid");

        // 3 + 1 + 1 = 5 tasks owned by us.
        assert_eq!(count_tasks_under(root, uid), Some(5));
        // A uid we don't own matches nothing (the synthetic dirs are ours).
        assert_eq!(count_tasks_under(root, uid.wrapping_add(1)), Some(0));
    }
}
