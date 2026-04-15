use std::cell::UnsafeCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::arch::{wait_time_start, wait_time_to_ns, wait_time_total_to_ns};

const DEFAULT_TIMING_SAMPLE_STRIDE: u64 = 8;
const TIMING_SAMPLE_STRIDE_ENV: &str = "LB_SIMPLE_TIMING_SAMPLE_STRIDE";
const OUTSIDE_SAMPLE_STRIDE_ENV: &str = "LB_SIMPLE_OUTSIDE_SAMPLE_STRIDE";

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
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ThreadStatsAux {
    thread_start_sample: u64,
    thread_elapsed_total: u64,
    wait_total: u64,
    wait_sample_count: u64,
    wait_start_sample: u64,
    wait_end_sample: u64,
    hold_total: u64,
    hold_sample_count: u64,
    hold_start_sample: u64,
    lock_count: u64,
    outside_gap_total: u64,
    outside_gap_samples: u64,
    sample_countdown: u64,
    op_sample_decided: bool,
    op_sampled: bool,
    outside_sample_countdown: u64,
    outside_sample_pending: bool,
    outside_unlock_sample: u64,
    outside_wait_start_sample: u64,
    outside_wait_total: u64,
}

impl ThreadStatsAux {
    const fn new() -> Self {
        Self {
            thread_start_sample: 0,
            thread_elapsed_total: 0,
            wait_total: 0,
            wait_sample_count: 0,
            wait_start_sample: 0,
            wait_end_sample: 0,
            hold_total: 0,
            hold_sample_count: 0,
            hold_start_sample: 0,
            lock_count: 0,
            outside_gap_total: 0,
            outside_gap_samples: 0,
            sample_countdown: 0,
            op_sample_decided: false,
            op_sampled: false,
            outside_sample_countdown: 0,
            outside_sample_pending: false,
            outside_unlock_sample: 0,
            outside_wait_start_sample: 0,
            outside_wait_total: 0,
        }
    }
}

thread_local! {
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
    static THREAD_AUX: UnsafeCell<ThreadStatsAux> = const { UnsafeCell::new(ThreadStatsAux::new()) };
}

static PROCESS_THREAD_ELAPSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_WAIT_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_HOLD_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_LOCK_COUNT: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_GAP_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_GAP_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[inline(always)]
fn thread_aux() -> *mut ThreadStatsAux {
    THREAD_AUX.with(|aux| aux.get())
}

#[inline(always)]
fn refresh_thread_elapsed_for_aux(aux: &mut ThreadStatsAux, now_sample: u64) {
    if aux.thread_start_sample == 0 {
        aux.thread_start_sample = now_sample;
        aux.thread_elapsed_total = 0;
        return;
    }
    aux.thread_elapsed_total = now_sample.saturating_sub(aux.thread_start_sample);
}

#[inline(always)]
fn ensure_thread_start_sample(aux: &mut ThreadStatsAux, sample: u64) {
    if aux.thread_start_sample == 0 {
        aux.thread_start_sample = sample;
    }
}

fn parse_timing_sample_stride(value: Option<&str>) -> u64 {
    let Some(value) = value else {
        return DEFAULT_TIMING_SAMPLE_STRIDE;
    };

    match value.trim().parse::<u64>() {
        Ok(stride) if stride > 0 => stride,
        _ => DEFAULT_TIMING_SAMPLE_STRIDE,
    }
}

#[inline(always)]
fn advance_sample_countdown(countdown: &mut u64, stride: u64) -> bool {
    let sampled = *countdown == 0;
    *countdown = if sampled { stride - 1 } else { *countdown - 1 };
    sampled
}

#[inline(always)]
fn timing_sample_stride() -> u64 {
    static TIMING_SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *TIMING_SAMPLE_STRIDE.get_or_init(|| {
        parse_timing_sample_stride(std::env::var(TIMING_SAMPLE_STRIDE_ENV).ok().as_deref())
    })
}

#[inline(always)]
fn outside_sample_stride() -> u64 {
    static OUTSIDE_SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *OUTSIDE_SAMPLE_STRIDE.get_or_init(|| {
        std::env::var(OUTSIDE_SAMPLE_STRIDE_ENV)
            .ok()
            .as_deref()
            .map_or_else(timing_sample_stride, |value| {
                let parsed = parse_timing_sample_stride(Some(value));
                if parsed == DEFAULT_TIMING_SAMPLE_STRIDE && value.trim() != "8" {
                    timing_sample_stride()
                } else {
                    parsed
                }
            })
    })
}

#[inline(always)]
fn begin_lock_timing_sample(aux: &mut ThreadStatsAux) -> bool {
    if aux.op_sample_decided {
        return aux.op_sampled;
    }

    let sampled = advance_sample_countdown(&mut aux.sample_countdown, timing_sample_stride());
    aux.op_sample_decided = true;
    aux.op_sampled = sampled;
    sampled
}

#[inline(always)]
fn finish_lock_timing_sample(aux: &mut ThreadStatsAux) {
    aux.op_sample_decided = false;
    aux.op_sampled = false;
}

#[inline(always)]
fn begin_outside_gap_sample(aux: &mut ThreadStatsAux) -> bool {
    advance_sample_countdown(&mut aux.outside_sample_countdown, outside_sample_stride())
}

#[inline(always)]
fn finish_outside_gap_sample(aux: &mut ThreadStatsAux) {
    aux.outside_sample_pending = false;
    aux.outside_unlock_sample = 0;
    aux.outside_wait_start_sample = 0;
    aux.outside_wait_total = 0;
}

#[inline(always)]
fn convert_optional_sample_to_ns(sample: u64) -> u64 {
    if sample == 0 {
        0
    } else {
        wait_time_to_ns(sample)
    }
}

#[inline(always)]
fn sync_aux_to_ctx(ctx: &mut LockSchedThreadCtx, aux: &ThreadStatsAux) {
    ctx.thread_start_ns = convert_optional_sample_to_ns(aux.thread_start_sample);
    ctx.thread_elapsed_ns_total = wait_time_total_to_ns(aux.thread_elapsed_total);
    ctx.wait_ns_total = wait_time_total_to_ns(aux.wait_total);
    ctx.wait_start_ns = convert_optional_sample_to_ns(aux.wait_start_sample);
    ctx.hold_ns_total = wait_time_total_to_ns(aux.hold_total);
    ctx.hold_start_ns = convert_optional_sample_to_ns(aux.hold_start_sample);
    ctx.lock_count = aux.lock_count;
}

#[inline(always)]
pub fn record_thread_start() {
    let now_sample = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        if aux.thread_start_sample == 0 {
            aux.thread_start_sample = now_sample;
            aux.thread_elapsed_total = 0;
        }
    }
}

#[inline(always)]
pub fn flush_current_thread_stats() {
    let now_sample = wait_time_start();
    unsafe {
        let ctx = &mut *thread_ctx();
        let aux = &mut *thread_aux();
        refresh_thread_elapsed_for_aux(aux, now_sample);
        sync_aux_to_ctx(ctx, aux);

        if aux.thread_elapsed_total == 0
            && aux.wait_total == 0
            && aux.hold_total == 0
            && aux.lock_count == 0
            && aux.outside_gap_total == 0
            && aux.outside_gap_samples == 0
        {
            return;
        }

        PROCESS_THREAD_ELAPSED_TOTAL.fetch_add(aux.thread_elapsed_total, Ordering::Relaxed);
        PROCESS_WAIT_TOTAL.fetch_add(aux.wait_total, Ordering::Relaxed);
        PROCESS_HOLD_TOTAL.fetch_add(aux.hold_total, Ordering::Relaxed);
        PROCESS_LOCK_COUNT.fetch_add(aux.lock_count, Ordering::Relaxed);
        PROCESS_OUTSIDE_GAP_TOTAL.fetch_add(aux.outside_gap_total, Ordering::Relaxed);
        PROCESS_OUTSIDE_GAP_SAMPLES.fetch_add(aux.outside_gap_samples, Ordering::Relaxed);

        *aux = ThreadStatsAux::new();
    }
}

pub fn print_process_stats(label: &str) {
    flush_current_thread_stats();

    let thread_elapsed_total = PROCESS_THREAD_ELAPSED_TOTAL.load(Ordering::Relaxed);
    let wait_total = PROCESS_WAIT_TOTAL.load(Ordering::Relaxed);
    let hold_total = PROCESS_HOLD_TOTAL.load(Ordering::Relaxed);
    let lock_count = PROCESS_LOCK_COUNT.load(Ordering::Relaxed);
    let outside_gap_total = PROCESS_OUTSIDE_GAP_TOTAL.load(Ordering::Relaxed);
    let outside_gap_samples = PROCESS_OUTSIDE_GAP_SAMPLES.load(Ordering::Relaxed);

    let thread_elapsed_ns_total = wait_time_total_to_ns(thread_elapsed_total);
    let wait_ns_total = wait_time_total_to_ns(wait_total);
    let hold_ns_total = wait_time_total_to_ns(hold_total);
    let outside_ns_gap_total = wait_time_total_to_ns(outside_gap_total);

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
    let avg_outside_ns = if outside_gap_samples != 0 {
        outside_ns_gap_total as f64 / outside_gap_samples as f64
    } else {
        0.0
    };

    println!("stats_label: {label}");
    println!("avg_critical_ns: {avg_critical_ns:.2}");
    println!("avg_outside_ns: {avg_outside_ns:.2}");
    println!("avg_outside_ns_elapsed: {avg_outside_ns_elapsed:.2}");
    println!("outside_ns_unlock_gap_samples: {outside_gap_samples}");
}

#[inline(always)]
pub fn record_wait_start() -> u64 {
    unsafe {
        let aux = &mut *thread_aux();
        if !begin_lock_timing_sample(aux) && !aux.outside_sample_pending {
            return 0;
        }
    }

    let wait_start = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, wait_start);
        if aux.op_sampled {
            aux.wait_start_sample = wait_start;
        }
        if aux.outside_sample_pending && aux.outside_wait_start_sample == 0 {
            aux.outside_wait_start_sample = wait_start;
        }
    }
    wait_start
}

#[inline(always)]
pub fn record_wait_end(wait_start: u64) {
    if wait_start == 0 {
        return;
    }

    let wait_end = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, wait_end);
        if aux.op_sampled {
            aux.wait_end_sample = wait_end;
        }
        if aux.outside_sample_pending && aux.outside_wait_start_sample != 0 {
            aux.outside_wait_total = wait_end.saturating_sub(aux.outside_wait_start_sample);
        }
    }
}

#[inline(always)]
pub fn record_lock_acquired() {
    unsafe {
        let aux = &mut *thread_aux();
        if !begin_lock_timing_sample(aux) && !aux.outside_sample_pending {
            return;
        }
    }

    let hold_start = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, hold_start);
        if aux.op_sampled {
            aux.hold_start_sample = hold_start;
        }
        if aux.outside_sample_pending {
            let outside_gap = hold_start.saturating_sub(aux.outside_unlock_sample);
            aux.outside_gap_total += outside_gap.saturating_sub(aux.outside_wait_total);
            aux.outside_gap_samples += 1;
            finish_outside_gap_sample(aux);
        }
    }
}

#[inline(always)]
pub fn record_hold_end_sample() -> u64 {
    unsafe {
        if !(*thread_aux()).op_sampled {
            return 0;
        }
    }
    wait_time_start()
}

#[inline(always)]
pub fn record_post_unlock(hold_end: u64) {
    unsafe {
        let aux = &mut *thread_aux();
        aux.lock_count += 1;

        if hold_end != 0 {
            refresh_thread_elapsed_for_aux(aux, hold_end);

            if aux.wait_start_sample != 0 {
                let wait_end = if aux.wait_end_sample != 0 {
                    aux.wait_end_sample
                } else {
                    aux.hold_start_sample
                };
                let wait_total = wait_end.saturating_sub(aux.wait_start_sample);
                aux.wait_total += wait_total.saturating_mul(timing_sample_stride());
                aux.wait_sample_count += 1;
            }

            if aux.hold_start_sample != 0 {
                aux.hold_total += hold_end
                    .saturating_sub(aux.hold_start_sample)
                    .saturating_mul(timing_sample_stride());
                aux.hold_sample_count += 1;
            }
        }

        aux.wait_start_sample = 0;
        aux.wait_end_sample = 0;
        aux.hold_start_sample = 0;

        finish_lock_timing_sample(aux);
        if !aux.outside_sample_pending && begin_outside_gap_sample(aux) {
            aux.outside_sample_pending = true;
            aux.outside_unlock_sample = wait_time_start();
            aux.outside_wait_start_sample = 0;
            aux.outside_wait_total = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        begin_lock_timing_sample, begin_outside_gap_sample, finish_outside_gap_sample,
        outside_sample_stride, parse_timing_sample_stride,
        record_post_unlock, refresh_thread_elapsed_for_aux, ThreadStatsAux,
        DEFAULT_TIMING_SAMPLE_STRIDE,
    };

    #[test]
    fn refresh_thread_elapsed_tracks_raw_wait_time_units() {
        let mut aux = ThreadStatsAux::new();

        refresh_thread_elapsed_for_aux(&mut aux, 100);
        assert_eq!(aux.thread_start_sample, 100);
        assert_eq!(aux.thread_elapsed_total, 0);

        refresh_thread_elapsed_for_aux(&mut aux, 165);
        assert_eq!(aux.thread_start_sample, 100);
        assert_eq!(aux.thread_elapsed_total, 65);
    }

    #[test]
    fn timing_sampling_uses_default_stride_of_eight() {
        let mut aux = ThreadStatsAux::new();
        let mut sampled = Vec::new();

        for _ in 0..16 {
            sampled.push(begin_lock_timing_sample(&mut aux));
            super::finish_lock_timing_sample(&mut aux);
        }

        assert_eq!(
            sampled,
            vec![
                true, false, false, false, false, false, false, false, true, false, false,
                false, false, false, false, false
            ]
        );
    }

    #[test]
    fn parse_timing_sample_stride_uses_default_for_missing_or_invalid_values() {
        assert_eq!(parse_timing_sample_stride(None), DEFAULT_TIMING_SAMPLE_STRIDE);
        assert_eq!(parse_timing_sample_stride(Some("")), DEFAULT_TIMING_SAMPLE_STRIDE);
        assert_eq!(parse_timing_sample_stride(Some("0")), DEFAULT_TIMING_SAMPLE_STRIDE);
        assert_eq!(parse_timing_sample_stride(Some("abc")), DEFAULT_TIMING_SAMPLE_STRIDE);
    }

    #[test]
    fn parse_timing_sample_stride_accepts_positive_values() {
        assert_eq!(parse_timing_sample_stride(Some("4")), 4);
        assert_eq!(parse_timing_sample_stride(Some(" 16 ")), 16);
    }

    #[test]
    fn outside_stride_defaults_to_timing_stride_when_unset() {
        assert_eq!(outside_sample_stride(), DEFAULT_TIMING_SAMPLE_STRIDE);
    }

    #[test]
    fn outside_sampling_uses_default_stride_of_eight() {
        let mut aux = ThreadStatsAux::new();
        let mut sampled = Vec::new();

        for _ in 0..16 {
            sampled.push(begin_outside_gap_sample(&mut aux));
        }

        assert_eq!(
            sampled,
            vec![
                true, false, false, false, false, false, false, false, true, false, false,
                false, false, false, false, false
            ]
        );
    }

    #[test]
    fn finish_outside_gap_sample_clears_pending_state() {
        let mut aux = ThreadStatsAux {
            outside_sample_pending: true,
            outside_unlock_sample: 100,
            outside_wait_start_sample: 120,
            outside_wait_total: 30,
            ..ThreadStatsAux::new()
        };

        finish_outside_gap_sample(&mut aux);

        assert!(!aux.outside_sample_pending);
        assert_eq!(aux.outside_unlock_sample, 0);
        assert_eq!(aux.outside_wait_start_sample, 0);
        assert_eq!(aux.outside_wait_total, 0);
    }

    #[test]
    fn post_unlock_batches_wait_hold_and_gap_updates() {
        let ctx = super::thread_ctx();
        let aux = super::thread_aux();

        unsafe {
            *ctx = super::LockSchedThreadCtx::new();
            *aux = ThreadStatsAux {
                thread_start_sample: 100,
                wait_start_sample: 130,
                wait_end_sample: 180,
                hold_start_sample: 180,
                op_sample_decided: true,
                op_sampled: true,
                ..ThreadStatsAux::new()
            };
        }

        record_post_unlock(260);

        unsafe {
            assert_eq!((*aux).thread_elapsed_total, 160);
            assert_eq!((*aux).wait_total, 50 * super::DEFAULT_TIMING_SAMPLE_STRIDE);
            assert_eq!((*aux).wait_sample_count, 1);
            assert_eq!((*aux).hold_total, 80 * super::DEFAULT_TIMING_SAMPLE_STRIDE);
            assert_eq!((*aux).hold_sample_count, 1);
            assert_eq!((*aux).lock_count, 1);
            assert_eq!((*aux).outside_gap_total, 0);
            assert_eq!((*aux).outside_gap_samples, 0);
            assert_eq!((*aux).wait_start_sample, 0);
            assert_eq!((*aux).wait_end_sample, 0);
            assert_eq!((*aux).hold_start_sample, 0);
            assert!(!(*aux).op_sample_decided);
            assert!(!(*aux).op_sampled);
            assert!((*aux).outside_sample_pending);
            assert_eq!((*aux).outside_wait_total, 0);
        }
    }

    #[test]
    fn unsampled_post_unlock_counts_lock_without_timing_totals() {
        let aux = super::thread_aux();

        unsafe {
            *aux = ThreadStatsAux {
                op_sample_decided: true,
                op_sampled: false,
                ..ThreadStatsAux::new()
            };
        }

        record_post_unlock(0);

        unsafe {
            assert_eq!((*aux).lock_count, 1);
            assert_eq!((*aux).wait_total, 0);
            assert_eq!((*aux).hold_total, 0);
            assert_eq!((*aux).outside_gap_total, 0);
            assert!(!(*aux).op_sample_decided);
            assert!(!(*aux).op_sampled);
        }
    }

    #[test]
    fn outside_gap_pairs_sampled_unlock_with_next_acquire() {
        let aux = super::thread_aux();

        unsafe {
            *aux = ThreadStatsAux {
                outside_sample_pending: true,
                outside_unlock_sample: 100,
                outside_wait_total: 20,
                ..ThreadStatsAux::new()
            };
        }

        super::record_lock_acquired();

        unsafe {
            assert!((*aux).outside_gap_total > 0);
            assert_eq!((*aux).outside_gap_samples, 1);
            assert!(!(*aux).outside_sample_pending);
            assert_eq!((*aux).outside_unlock_sample, 0);
            assert_eq!((*aux).outside_wait_start_sample, 0);
            assert_eq!((*aux).outside_wait_total, 0);
        }
    }
}
