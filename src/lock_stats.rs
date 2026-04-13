use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::arch::{wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};

pub const ADMISSION_CPU_NONE: u32 = u32::MAX;

/// Per-thread lock scheduling context, read by BPF via bpf_probe_read_user.
#[repr(C)]
pub struct LockSchedThreadCtx {
    pub thread_start_ns: u64,
    pub thread_elapsed_ns_total: u64,
    pub wait_ns_total: u64,
    pub wait_start_ns: u64,
    pub hold_ns_total: u64,
    pub hold_start_ns: u64,
    pub lock_count: u64,
    pub admission_owned: u32,
    pub admission_cpu: u32,
    pub admission_requeue_home: u32,
    pub in_critical_section: u32,
    pub slow_path_pending: u32,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            thread_start_ns: 0,
            thread_elapsed_ns_total: 0,
            wait_ns_total: 0,
            wait_start_ns: 0,
            hold_ns_total: 0,
            hold_start_ns: 0,
            lock_count: 0,
            admission_owned: 0,
            admission_cpu: ADMISSION_CPU_NONE,
            admission_requeue_home: 0,
            in_critical_section: 0,
            slow_path_pending: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ThreadStatsAux {
    pending_wait_ns: u64,
    outside_ns_gap_total: u64,
    outside_ns_gap_samples: u64,
    last_unlock_ns: u64,
}

impl ThreadStatsAux {
    const fn new() -> Self {
        Self {
            pending_wait_ns: 0,
            outside_ns_gap_total: 0,
            outside_ns_gap_samples: 0,
            last_unlock_ns: 0,
        }
    }
}

thread_local! {
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
    static THREAD_AUX: UnsafeCell<ThreadStatsAux> = const { UnsafeCell::new(ThreadStatsAux::new()) };
}

static PROCESS_THREAD_ELAPSED_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_WAIT_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_HOLD_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_LOCK_COUNT: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_NS_GAP_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_NS_GAP_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionStateSnapshot {
    pub admission_owned: bool,
    pub admission_cpu: u32,
    pub admission_requeue_home: bool,
    pub in_critical_section: bool,
    pub slow_path_pending: bool,
}

#[inline(always)]
pub fn thread_has_admission() -> bool {
    unsafe { (*thread_ctx()).admission_owned != 0 }
}

#[inline(always)]
pub fn mark_slow_path_pending() {
    unsafe {
        let ctx = thread_ctx();
        (*ctx).slow_path_pending = 1;
        (*ctx).in_critical_section = 0;
    }
}

#[inline(always)]
pub fn grant_slow_path_admission(cpu: u32) {
    unsafe {
        let ctx = thread_ctx();
        (*ctx).admission_owned = 1;
        (*ctx).admission_cpu = cpu;
        (*ctx).admission_requeue_home = 0;
    }
}

#[inline(always)]
pub fn mark_critical_section_entered() {
    unsafe {
        let ctx = thread_ctx();
        (*ctx).slow_path_pending = 0;
        (*ctx).in_critical_section = 1;
        (*ctx).admission_requeue_home = 0;
    }
}

#[inline(always)]
pub fn clear_admission_state() {
    unsafe {
        let ctx = thread_ctx();
        (*ctx).admission_owned = 0;
        (*ctx).admission_cpu = ADMISSION_CPU_NONE;
        (*ctx).admission_requeue_home = 0;
        (*ctx).in_critical_section = 0;
        (*ctx).slow_path_pending = 0;
    }
}

#[inline(always)]
pub fn admission_state_snapshot() -> AdmissionStateSnapshot {
    unsafe {
        let ctx = thread_ctx();
        AdmissionStateSnapshot {
            admission_owned: (*ctx).admission_owned != 0,
            admission_cpu: (*ctx).admission_cpu,
            admission_requeue_home: (*ctx).admission_requeue_home != 0,
            in_critical_section: (*ctx).in_critical_section != 0,
            slow_path_pending: (*ctx).slow_path_pending != 0,
        }
    }
}

#[inline(always)]
fn thread_aux() -> *mut ThreadStatsAux {
    THREAD_AUX.with(|aux| aux.get())
}

#[inline(always)]
fn refresh_thread_elapsed_ns_for_ctx(ctx: *mut LockSchedThreadCtx, now_ns: u64) {
    let thread_start_ns = unsafe { (*ctx).thread_start_ns };
    if thread_start_ns == 0 {
        unsafe {
            (*ctx).thread_start_ns = now_ns;
            (*ctx).thread_elapsed_ns_total = 0;
        }
        return;
    }
    unsafe {
        (*ctx).thread_elapsed_ns_total = now_ns.saturating_sub(thread_start_ns);
    }
}

#[inline(always)]
pub fn record_thread_start() {
    let now_ns = wait_time_to_ns(wait_time_start());
    unsafe {
        let ctx = thread_ctx();
        if (*ctx).thread_start_ns == 0 {
            (*ctx).thread_start_ns = now_ns;
            (*ctx).thread_elapsed_ns_total = 0;
        }
    }
}

#[inline(always)]
pub fn flush_current_thread_stats() {
    let now_ns = wait_time_to_ns(wait_time_start());
    unsafe {
        let ctx = thread_ctx();
        let aux = thread_aux();
        refresh_thread_elapsed_ns_for_ctx(ctx, now_ns);

        let thread_elapsed_ns_total = (*ctx).thread_elapsed_ns_total;
        let wait_ns_total = (*ctx).wait_ns_total;
        let hold_ns_total = (*ctx).hold_ns_total;
        let lock_count = (*ctx).lock_count;
        let outside_ns_gap_total = (*aux).outside_ns_gap_total;
        let outside_ns_gap_samples = (*aux).outside_ns_gap_samples;

        if thread_elapsed_ns_total == 0
            && wait_ns_total == 0
            && hold_ns_total == 0
            && lock_count == 0
            && outside_ns_gap_total == 0
            && outside_ns_gap_samples == 0
        {
            return;
        }

        PROCESS_THREAD_ELAPSED_NS_TOTAL.fetch_add(thread_elapsed_ns_total, Ordering::Relaxed);
        PROCESS_WAIT_NS_TOTAL.fetch_add(wait_ns_total, Ordering::Relaxed);
        PROCESS_HOLD_NS_TOTAL.fetch_add(hold_ns_total, Ordering::Relaxed);
        PROCESS_LOCK_COUNT.fetch_add(lock_count, Ordering::Relaxed);
        PROCESS_OUTSIDE_NS_GAP_TOTAL.fetch_add(outside_ns_gap_total, Ordering::Relaxed);
        PROCESS_OUTSIDE_NS_GAP_SAMPLES.fetch_add(outside_ns_gap_samples, Ordering::Relaxed);

        *ctx = LockSchedThreadCtx::new();
        *aux = ThreadStatsAux::new();
    }
}

pub fn print_process_stats(label: &str) {
    flush_current_thread_stats();

    let thread_elapsed_ns_total = PROCESS_THREAD_ELAPSED_NS_TOTAL.load(Ordering::Relaxed);
    let wait_ns_total = PROCESS_WAIT_NS_TOTAL.load(Ordering::Relaxed);
    let hold_ns_total = PROCESS_HOLD_NS_TOTAL.load(Ordering::Relaxed);
    let lock_count = PROCESS_LOCK_COUNT.load(Ordering::Relaxed);
    let outside_ns_gap_total = PROCESS_OUTSIDE_NS_GAP_TOTAL.load(Ordering::Relaxed);
    let outside_ns_gap_samples = PROCESS_OUTSIDE_NS_GAP_SAMPLES.load(Ordering::Relaxed);

    let avg_critical_ns = if lock_count != 0 {
        hold_ns_total as f64 / lock_count as f64
    } else {
        0.0
    };
    let avg_outside_ns_elapsed = if lock_count != 0 {
        thread_elapsed_ns_total.saturating_sub(wait_ns_total.saturating_add(hold_ns_total)) as f64
            / lock_count as f64
    } else {
        0.0
    };
    let avg_outside_ns = if outside_ns_gap_samples != 0 {
        outside_ns_gap_total as f64 / outside_ns_gap_samples as f64
    } else {
        0.0
    };

    println!("stats_label: {label}");
    println!("avg_critical_ns: {avg_critical_ns:.2}");
    println!("avg_outside_ns: {avg_outside_ns:.2}");
    println!("avg_outside_ns_elapsed: {avg_outside_ns_elapsed:.2}");
    println!("outside_ns_unlock_gap_samples: {outside_ns_gap_samples}");
}

#[inline(always)]
pub fn record_wait_start() -> u64 {
    let wait_start = wait_time_start();
    let wait_start_ns = wait_time_to_ns(wait_start);
    unsafe {
        let ctx = thread_ctx();
        refresh_thread_elapsed_ns_for_ctx(ctx, wait_start_ns);
        (*ctx).wait_start_ns = wait_start_ns;
    }
    wait_start
}

#[inline(always)]
pub fn record_wait_end(wait_start: u64) {
    let wait_end = wait_time_start();
    let wait_end_ns = wait_time_to_ns(wait_end);
    unsafe {
        let ctx = thread_ctx();
        let aux = thread_aux();
        let wait_ns = wait_time_elapsed_ns_between(wait_start, wait_end);
        refresh_thread_elapsed_ns_for_ctx(ctx, wait_end_ns);
        (*ctx).wait_ns_total += wait_ns;
        (*aux).pending_wait_ns = wait_ns;
    }
}

#[inline(always)]
pub fn record_lock_acquired() {
    let hold_start_ns = wait_time_to_ns(wait_time_start());
    unsafe {
        let ctx = thread_ctx();
        let aux = thread_aux();
        refresh_thread_elapsed_ns_for_ctx(ctx, hold_start_ns);
        record_outside_gap_sample(&mut *aux, hold_start_ns);
        (*ctx).hold_start_ns = hold_start_ns;
    }
}

#[inline(always)]
pub fn record_hold_end() {
    let hold_end_ns = wait_time_to_ns(wait_time_start());
    unsafe {
        let ctx = thread_ctx();
        let aux = thread_aux();
        refresh_thread_elapsed_ns_for_ctx(ctx, hold_end_ns);
        let hold_start_ns = (*ctx).hold_start_ns;
        if hold_start_ns == 0 {
            return;
        }
        (*ctx).hold_ns_total += hold_end_ns.saturating_sub(hold_start_ns);
        (*ctx).hold_start_ns = 0;
        (*ctx).lock_count += 1;
        (*aux).last_unlock_ns = hold_end_ns;
    }
}

#[inline(always)]
fn record_outside_gap_sample(aux: &mut ThreadStatsAux, hold_start_ns: u64) {
    if aux.last_unlock_ns != 0 {
        let unlock_gap_ns = hold_start_ns.saturating_sub(aux.last_unlock_ns);
        aux.outside_ns_gap_total += unlock_gap_ns.saturating_sub(aux.pending_wait_ns);
        aux.outside_ns_gap_samples += 1;
    }
    aux.pending_wait_ns = 0;
}

#[cfg(test)]
mod tests {
    use super::{
        ADMISSION_CPU_NONE, ThreadStatsAux, admission_state_snapshot,
        clear_admission_state, grant_slow_path_admission, mark_critical_section_entered,
        mark_slow_path_pending, record_outside_gap_sample, thread_ctx,
    };

    fn reset_thread_ctx_for_test() {
        unsafe {
            *thread_ctx() = super::LockSchedThreadCtx::new();
        }
    }

    #[test]
    fn outside_gap_uses_previous_unlock_and_current_wait() {
        let mut aux = ThreadStatsAux::default();

        aux.last_unlock_ns = 150;

        aux.pending_wait_ns = 20;
        record_outside_gap_sample(&mut aux, 240);

        assert_eq!(aux.outside_ns_gap_total, 70);
        assert_eq!(aux.outside_ns_gap_samples, 1);
        assert_eq!(aux.pending_wait_ns, 0);
    }

    #[test]
    fn outside_gap_skips_first_acquire_without_previous_unlock() {
        let mut aux = ThreadStatsAux::default();

        aux.pending_wait_ns = 15;
        record_outside_gap_sample(&mut aux, 80);

        assert_eq!(aux.outside_ns_gap_total, 0);
        assert_eq!(aux.outside_ns_gap_samples, 0);
        assert_eq!(aux.pending_wait_ns, 0);
    }

    #[test]
    fn outside_gap_saturates_when_wait_exceeds_unlock_gap() {
        let mut aux = ThreadStatsAux {
            pending_wait_ns: 80,
            outside_ns_gap_total: 0,
            outside_ns_gap_samples: 0,
            last_unlock_ns: 100,
        };

        record_outside_gap_sample(&mut aux, 150);

        assert_eq!(aux.outside_ns_gap_total, 0);
        assert_eq!(aux.outside_ns_gap_samples, 1);
        assert_eq!(aux.pending_wait_ns, 0);
    }

    #[test]
    fn admission_helpers_track_pending_grant_and_release() {
        reset_thread_ctx_for_test();

        mark_slow_path_pending();
        assert_eq!(
            admission_state_snapshot(),
            super::AdmissionStateSnapshot {
                admission_owned: false,
                admission_cpu: ADMISSION_CPU_NONE,
                admission_requeue_home: false,
                in_critical_section: false,
                slow_path_pending: true,
            }
        );

        grant_slow_path_admission(7);
        assert_eq!(
            admission_state_snapshot(),
            super::AdmissionStateSnapshot {
                admission_owned: true,
                admission_cpu: 7,
                admission_requeue_home: false,
                in_critical_section: false,
                slow_path_pending: true,
            }
        );

        mark_critical_section_entered();
        assert_eq!(
            admission_state_snapshot(),
            super::AdmissionStateSnapshot {
                admission_owned: true,
                admission_cpu: 7,
                admission_requeue_home: false,
                in_critical_section: true,
                slow_path_pending: false,
            }
        );

        clear_admission_state();
        assert_eq!(
            admission_state_snapshot(),
            super::AdmissionStateSnapshot {
                admission_owned: false,
                admission_cpu: ADMISSION_CPU_NONE,
                admission_requeue_home: false,
                in_critical_section: false,
                slow_path_pending: false,
            }
        );
    }
}
