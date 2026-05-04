use std::cell::UnsafeCell;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::admission;
use crate::arch::{
    wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns, wait_time_total_to_ns,
};
use crate::cpu_affinity;

const SAMPLE_STRIDE_ENV: &str = "ACCORDIN_SAMPLE_STRIDE";
const DYNAMIC_CPU_WINDOW_NS_ENV: &str = "ACCORDIN_DYNAMIC_CPU_WINDOW_NS";

const DEFAULT_SAMPLE_STRIDE: u64 = 8;
const DEFAULT_DYNAMIC_CPU_WINDOW_NS: u64 = 1_000_000;
const DYNAMIC_CPU_CRITICAL_EWMA_ALPHA: f64 = 0.5;
const DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA: f64 = 0.1;
const DYNAMIC_CPU_MAX_TARGET_MULTIPLIER: usize = 8;

/// Per-thread lock statistics storage used by print_process_stats.
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
    outside_sample_pending: bool,
    outside_unlock_sample: u64,
    outside_wait_start_sample: u64,
    outside_wait_total: u64,
    dynamic_sample_pending: bool,
    dynamic_pending_outside_ns: u64,
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
            outside_sample_pending: false,
            outside_unlock_sample: 0,
            outside_wait_start_sample: 0,
            outside_wait_total: 0,
            dynamic_sample_pending: false,
            dynamic_pending_outside_ns: 0,
        }
    }
}

#[derive(Default)]
struct WindowControlState {
    last_dynamic_cpu_count: Option<usize>,
    target_cpu_limit: Option<usize>,
    critical_ewma_ns: Option<f64>,
    outside_ewma_ns: Option<f64>,
    frozen: bool,
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

static PROCESS_CS_SUM_NS: AtomicU64 = AtomicU64::new(0);
static PROCESS_CS_COUNT: AtomicU64 = AtomicU64::new(0);
static PROCESS_NCS_SUM_NS: AtomicU64 = AtomicU64::new(0);
static PROCESS_NCS_COUNT: AtomicU64 = AtomicU64::new(0);
static DYNAMIC_CPU_LAST_TICK_NS: AtomicU64 = AtomicU64::new(0);

/// Returns a pointer to the current thread's stats context.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[inline(always)]
fn thread_aux() -> *mut ThreadStatsAux {
    THREAD_AUX.with(|aux| aux.get())
}

fn first_env_string(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_positive_u64(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().filter(|value| *value > 0)
}

fn parse_sample_stride(value: Option<&str>) -> u64 {
    value
        .and_then(parse_positive_u64)
        .unwrap_or(DEFAULT_SAMPLE_STRIDE)
}

fn parse_dynamic_cpu_window_ns(value: Option<&str>) -> u64 {
    value
        .and_then(parse_positive_u64)
        .unwrap_or(DEFAULT_DYNAMIC_CPU_WINDOW_NS)
}

#[inline(always)]
fn sampling_enabled() -> bool {
    static SAMPLING_ENABLED: OnceLock<bool> = OnceLock::new();

    *SAMPLING_ENABLED.get_or_init(cpu_affinity::configured_cpu_count_env_present)
}

#[inline(always)]
fn sample_stride() -> u64 {
    static SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *SAMPLE_STRIDE
        .get_or_init(|| parse_sample_stride(first_env_string(SAMPLE_STRIDE_ENV).as_deref()))
}

#[inline(always)]
fn dynamic_cpu_window_ns() -> u64 {
    static DYNAMIC_CPU_WINDOW_NS: OnceLock<u64> = OnceLock::new();

    *DYNAMIC_CPU_WINDOW_NS.get_or_init(|| {
        parse_dynamic_cpu_window_ns(first_env_string(DYNAMIC_CPU_WINDOW_NS_ENV).as_deref())
    })
}

fn dynamic_cpu_control_state() -> &'static Mutex<WindowControlState> {
    static DYNAMIC_CPU_CONTROL: OnceLock<Mutex<WindowControlState>> = OnceLock::new();

    DYNAMIC_CPU_CONTROL.get_or_init(|| Mutex::new(WindowControlState::default()))
}

#[inline(always)]
fn advance_periodic_sample(countdown: &mut u64, stride: u64) -> bool {
    let stride = stride.max(1);
    if *countdown == 0 {
        *countdown = stride.saturating_sub(1);
        true
    } else {
        *countdown -= 1;
        false
    }
}

#[inline(always)]
fn decide_operation_sampling(aux: &mut ThreadStatsAux) {
    decide_operation_sampling_with_enabled(aux, sampling_enabled());
}

#[inline(always)]
fn decide_operation_sampling_with_enabled(aux: &mut ThreadStatsAux, enabled: bool) {
    if aux.op_sample_decided {
        return;
    }

    aux.op_sampled = enabled && aux.outside_sample_pending;
    aux.op_sample_decided = true;
}

#[inline(always)]
fn finish_operation_sampling(aux: &mut ThreadStatsAux) {
    aux.op_sample_decided = false;
    aux.op_sampled = false;
}

#[inline(always)]
fn begin_outside_gap_sample(aux: &mut ThreadStatsAux) -> bool {
    begin_outside_gap_sample_with_enabled(aux, sampling_enabled())
}

#[inline(always)]
fn begin_outside_gap_sample_with_enabled(aux: &mut ThreadStatsAux, enabled: bool) -> bool {
    enabled && advance_periodic_sample(&mut aux.sample_countdown, sample_stride())
}

#[inline(always)]
fn finish_outside_gap_sample(aux: &mut ThreadStatsAux) {
    aux.outside_sample_pending = false;
    aux.outside_unlock_sample = 0;
    aux.outside_wait_start_sample = 0;
    aux.outside_wait_total = 0;
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

fn reset_thread_measurement_state(ctx: &mut LockSchedThreadCtx, aux: &mut ThreadStatsAux) {
    let now_sample = wait_time_start();
    *aux = ThreadStatsAux::new();
    aux.thread_start_sample = now_sample;

    ctx.thread_start_ns = wait_time_to_ns(now_sample);
    ctx.thread_elapsed_ns_total = 0;
    ctx.wait_ns_total = 0;
    ctx.wait_start_ns = 0;
    ctx.hold_ns_total = 0;
    ctx.hold_start_ns = 0;
    ctx.lock_count = 0;
    admission::reset_state();
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

    if let Some(cpu_count) = cpu_affinity::current_dynamic_cpu_count() {
        println!("dynamic_cpu_affinity_cpus: {cpu_count}");
        println!("dynamic_cpu_window_ns: {}", dynamic_cpu_window_ns());
        println!("dynamic_cpu_critical_ewma_alpha: {DYNAMIC_CPU_CRITICAL_EWMA_ALPHA:.2}");
        println!("dynamic_cpu_outside_ewma_alpha: {DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA:.2}");
    }
}

#[inline(always)]
pub fn record_wait_start() -> u64 {
    unsafe {
        let aux = &mut *thread_aux();
        decide_operation_sampling(aux);
        if !aux.op_sampled {
            return 0;
        }
    }

    let wait_start = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, wait_start);
        aux.wait_start_sample = wait_start;
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
        aux.wait_start_sample = wait_start;
        aux.wait_end_sample = wait_end;
        if aux.outside_sample_pending && aux.outside_wait_start_sample != 0 {
            aux.outside_wait_total = wait_end.saturating_sub(aux.outside_wait_start_sample);
        }
    }
}

#[inline(always)]
pub fn record_lock_acquired() {
    admission::mark_critical_section_entered();

    unsafe {
        let aux = &mut *thread_aux();
        decide_operation_sampling(aux);
        if !aux.op_sampled {
            return;
        }
    }

    let hold_start = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, hold_start);
        aux.hold_start_sample = hold_start;

        if aux.outside_sample_pending {
            let wait_total = if aux.wait_start_sample != 0 {
                let wait_end = if aux.wait_end_sample != 0 {
                    aux.wait_end_sample
                } else {
                    hold_start
                };
                wait_end.saturating_sub(aux.wait_start_sample)
            } else {
                0
            };
            aux.outside_wait_total = wait_total;
            let outside_gap = hold_start.saturating_sub(aux.outside_unlock_sample);
            let outside_total = outside_gap.saturating_sub(wait_total);
            aux.outside_gap_total += outside_total;
            aux.outside_gap_samples += 1;
            aux.dynamic_sample_pending = true;
            aux.dynamic_pending_outside_ns = wait_time_total_to_ns(outside_total);

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
    let dynamic_control_enabled = sampling_enabled();
    let mut dynamic_control_now_ns = None;

    unsafe {
        let aux = &mut *thread_aux();
        aux.lock_count += 1;

        if hold_end != 0 {
            refresh_thread_elapsed_for_aux(aux, hold_end);
        }

        let wait_total = if hold_end != 0 && aux.wait_start_sample != 0 {
            let wait_end = if aux.wait_end_sample != 0 {
                aux.wait_end_sample
            } else {
                aux.hold_start_sample
            };
            wait_end.saturating_sub(aux.wait_start_sample)
        } else {
            0
        };
        if aux.op_sampled && wait_total != 0 {
            aux.wait_total += wait_total.saturating_mul(sample_stride());
            aux.wait_sample_count += 1;
        }
        aux.wait_start_sample = 0;
        aux.wait_end_sample = 0;

        let critical_total = if hold_end != 0 && aux.hold_start_sample != 0 {
            hold_end.saturating_sub(aux.hold_start_sample)
        } else {
            0
        };
        if aux.op_sampled && critical_total != 0 {
            aux.hold_total += critical_total.saturating_mul(sample_stride());
            aux.hold_sample_count += 1;
        }

        if aux.dynamic_sample_pending {
            if hold_end != 0 && aux.hold_start_sample != 0 {
                record_dynamic_control_sample(
                    wait_time_elapsed_ns_between(aux.hold_start_sample, hold_end),
                    aux.dynamic_pending_outside_ns,
                );
            }
            aux.dynamic_sample_pending = false;
            aux.dynamic_pending_outside_ns = 0;
        }

        aux.hold_start_sample = 0;
        finish_operation_sampling(aux);

        let begin_next_sample = !aux.outside_sample_pending && begin_outside_gap_sample(aux);
        let post_unlock_sample = if begin_next_sample || (dynamic_control_enabled && hold_end == 0)
        {
            wait_time_start()
        } else {
            hold_end
        };

        if begin_next_sample {
            aux.outside_sample_pending = true;
            aux.outside_unlock_sample = post_unlock_sample;
            aux.outside_wait_start_sample = 0;
            aux.outside_wait_total = 0;
        }

        if dynamic_control_enabled && post_unlock_sample != 0 {
            dynamic_control_now_ns = Some(wait_time_to_ns(post_unlock_sample));
        }
    }

    if let Some(now_ns) = dynamic_control_now_ns {
        maybe_run_dynamic_cpu_control(now_ns);
    }
}

#[inline(always)]
fn record_dynamic_control_sample(critical_ns: u64, outside_ns: u64) {
    PROCESS_CS_SUM_NS.fetch_add(critical_ns, Ordering::Relaxed);
    PROCESS_NCS_SUM_NS.fetch_add(outside_ns, Ordering::Relaxed);
    PROCESS_CS_COUNT.fetch_add(1, Ordering::Relaxed);
    PROCESS_NCS_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn maybe_run_dynamic_cpu_control(now_ns: u64) {
    let window_ns = dynamic_cpu_window_ns();
    let mut last_tick = DYNAMIC_CPU_LAST_TICK_NS.load(Ordering::Relaxed);

    loop {
        if last_tick == 0 {
            match DYNAMIC_CPU_LAST_TICK_NS.compare_exchange(
                0,
                now_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => {
                    last_tick = actual;
                    continue;
                }
            }
        }

        if now_ns.saturating_sub(last_tick) < window_ns {
            return;
        }

        match DYNAMIC_CPU_LAST_TICK_NS.compare_exchange(
            last_tick,
            now_ns,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => last_tick = actual,
        }
    }

    run_dynamic_cpu_control_tick();
}

fn run_dynamic_cpu_control_tick() {
    let Some((critical_avg_ns, outside_avg_ns)) = take_dynamic_sample_averages() else {
        return;
    };

    let mut control = dynamic_cpu_control_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if control.frozen {
        return;
    }

    let current_cpu_count = control
        .last_dynamic_cpu_count
        .or_else(cpu_affinity::current_dynamic_cpu_count);
    let target_cpu_limit = dynamic_cpu_target_limit(&mut control, current_cpu_count);
    let Some(smoothed_target) = dynamic_cpu_target_from_smoothed_components(
        &mut control,
        critical_avg_ns,
        outside_avg_ns,
        target_cpu_limit,
    ) else {
        return;
    };
    let rounded_target = dynamic_cpu_count_from_target(smoothed_target);

    if dynamic_cpu_count_within_dead_zone(current_cpu_count, rounded_target) {
        control.last_dynamic_cpu_count = current_cpu_count;
        return;
    }

    match cpu_affinity::update_dynamic_cpu_count(rounded_target) {
        Ok(Some(update)) => {
            control.last_dynamic_cpu_count = Some(update.applied_cpus);
            if update.changed {
                eprintln!(
                    "[lock_stats] dynamic CPU affinity set to {} CPUs (requested {})",
                    update.applied_cpus, update.requested_cpus
                );
            }
        }
        Ok(None) => {}
        Err(error) => log_dynamic_cpu_affinity_error(&error),
    }
}

fn take_dynamic_sample_averages() -> Option<(f64, f64)> {
    let critical_count = PROCESS_CS_COUNT.swap(0, Ordering::Relaxed);
    let outside_count = PROCESS_NCS_COUNT.swap(0, Ordering::Relaxed);
    let critical_sum = PROCESS_CS_SUM_NS.swap(0, Ordering::Relaxed);
    let outside_sum = PROCESS_NCS_SUM_NS.swap(0, Ordering::Relaxed);

    dynamic_sample_averages(critical_sum, critical_count, outside_sum, outside_count)
}

fn dynamic_sample_averages(
    critical_sum_ns: u64,
    critical_count: u64,
    outside_sum_ns: u64,
    outside_count: u64,
) -> Option<(f64, f64)> {
    if critical_count == 0 || outside_count == 0 {
        return None;
    }

    Some((
        critical_sum_ns as f64 / critical_count as f64,
        outside_sum_ns as f64 / outside_count as f64,
    ))
}

fn dynamic_cpu_target_from_sample_averages(critical_ns: f64, outside_ns: f64) -> Option<f64> {
    let valid_estimate =
        critical_ns.is_finite() && outside_ns.is_finite() && critical_ns > 0.0 && outside_ns >= 0.0;
    if !valid_estimate {
        return None;
    }

    let target = 1.0 + outside_ns / critical_ns;
    if !target.is_finite() {
        return None;
    }

    Some(target)
}

fn dynamic_cpu_target_from_smoothed_components(
    control: &mut WindowControlState,
    critical_ns: f64,
    outside_ns: f64,
    target_cpu_limit: Option<usize>,
) -> Option<f64> {
    let valid_sample =
        critical_ns.is_finite() && outside_ns.is_finite() && critical_ns > 0.0 && outside_ns >= 0.0;
    if !valid_sample {
        return None;
    }

    let target_cpu_limit = target_cpu_limit.map(|limit| limit.max(1) as f64);
    let max_outside_to_critical_ratio = target_cpu_limit.map(|limit| (limit - 1.0).max(0.0));
    let outside_ns = max_outside_to_critical_ratio
        .map(|ratio| outside_ns.min(critical_ns * ratio))
        .unwrap_or(outside_ns);

    let critical_ewma_ns = next_ewma(
        control.critical_ewma_ns,
        critical_ns,
        DYNAMIC_CPU_CRITICAL_EWMA_ALPHA,
    );
    let outside_ewma_ns = next_ewma(
        control.outside_ewma_ns,
        outside_ns,
        DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA,
    );
    let outside_ewma_ns = max_outside_to_critical_ratio
        .map(|ratio| outside_ewma_ns.min(critical_ewma_ns * ratio))
        .unwrap_or(outside_ewma_ns);
    let target = dynamic_cpu_target_from_sample_averages(critical_ewma_ns, outside_ewma_ns)?;
    let target = target_cpu_limit
        .map(|limit| target.min(limit))
        .unwrap_or(target);

    control.critical_ewma_ns = Some(critical_ewma_ns);
    control.outside_ewma_ns = Some(outside_ewma_ns);
    Some(target)
}

fn dynamic_cpu_target_limit(
    control: &mut WindowControlState,
    current_cpu_count: Option<usize>,
) -> Option<usize> {
    if control.target_cpu_limit.is_none() {
        control.target_cpu_limit = current_cpu_count.map(|count| {
            count
                .max(1)
                .saturating_mul(DYNAMIC_CPU_MAX_TARGET_MULTIPLIER)
        });
    }
    control.target_cpu_limit
}

fn next_ewma(previous: Option<f64>, value: f64, alpha: f64) -> f64 {
    match previous {
        Some(previous) => alpha * value + (1.0 - alpha) * previous,
        None => value,
    }
}

fn dynamic_cpu_count_from_target(target: f64) -> usize {
    target.round().clamp(1.0, usize::MAX as f64) as usize
}

fn dynamic_cpu_count_within_dead_zone(
    current_cpu_count: Option<usize>,
    candidate_cpu_count: usize,
) -> bool {
    current_cpu_count.is_some_and(|current| current.abs_diff(candidate_cpu_count) <= 1)
}

fn log_dynamic_cpu_affinity_error(error: &str) {
    static DYNAMIC_AFFINITY_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

    if !DYNAMIC_AFFINITY_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[lock_stats] dynamic CPU affinity update failed: {error}");
    }
}

pub fn dynamic_cpu_affinity_is_stable() -> bool {
    if !sampling_enabled() {
        return true;
    }

    let control = dynamic_cpu_control_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    control.last_dynamic_cpu_count.is_some() || cpu_affinity::current_dynamic_cpu_count().is_some()
}

pub fn dynamic_cpu_affinity_freeze() {
    if !sampling_enabled() {
        return;
    }

    let mut control = dynamic_cpu_control_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    control.frozen = true;
    if control.last_dynamic_cpu_count.is_none() {
        control.last_dynamic_cpu_count = cpu_affinity::current_dynamic_cpu_count();
    }
}

pub fn dynamic_cpu_affinity_begin_measurement_for_thread() {
    cpu_affinity::ensure_current_thread_affinity();

    unsafe {
        let ctx = &mut *thread_ctx();
        let aux = &mut *thread_aux();
        finish_outside_gap_sample(aux);
        reset_thread_measurement_state(ctx, aux);
    }
    unsafe {
        libc::sched_yield();
    }
    cpu_affinity::ensure_current_thread_affinity();
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SAMPLE_STRIDE, DYNAMIC_CPU_CRITICAL_EWMA_ALPHA, DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA,
        ThreadStatsAux, WindowControlState, advance_periodic_sample,
        begin_outside_gap_sample_with_enabled, decide_operation_sampling_with_enabled,
        dynamic_cpu_count_from_target, dynamic_cpu_count_within_dead_zone,
        dynamic_cpu_target_from_sample_averages, dynamic_cpu_target_from_smoothed_components,
        dynamic_cpu_target_limit, dynamic_sample_averages, finish_outside_gap_sample, next_ewma,
        parse_dynamic_cpu_window_ns, parse_sample_stride, record_lock_acquired, record_post_unlock,
        refresh_thread_elapsed_for_aux,
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
    fn sampling_uses_default_stride_of_eight() {
        let mut countdown = 0;
        let mut sampled = Vec::new();

        for _ in 0..16 {
            sampled.push(advance_periodic_sample(
                &mut countdown,
                DEFAULT_SAMPLE_STRIDE,
            ));
        }

        assert_eq!(
            sampled,
            vec![
                true, false, false, false, false, false, false, false, true, false, false, false,
                false, false, false, false
            ]
        );
    }

    #[test]
    fn parse_sample_stride_uses_default_for_missing_or_invalid_values() {
        assert_eq!(parse_sample_stride(None), DEFAULT_SAMPLE_STRIDE);
        assert_eq!(parse_sample_stride(Some("")), DEFAULT_SAMPLE_STRIDE);
        assert_eq!(parse_sample_stride(Some("0")), DEFAULT_SAMPLE_STRIDE);
        assert_eq!(parse_sample_stride(Some("abc")), DEFAULT_SAMPLE_STRIDE);
    }

    #[test]
    fn parse_sample_stride_accepts_positive_values() {
        assert_eq!(parse_sample_stride(Some("4")), 4);
        assert_eq!(parse_sample_stride(Some(" 16 ")), 16);
    }

    #[test]
    fn parse_dynamic_window_uses_default_for_missing_or_invalid_values() {
        assert_eq!(parse_dynamic_cpu_window_ns(None), 1_000_000);
        assert_eq!(parse_dynamic_cpu_window_ns(Some("0")), 1_000_000);
        assert_eq!(parse_dynamic_cpu_window_ns(Some("abc")), 1_000_000);
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
    fn sampling_gate_blocks_operation_samples_when_disabled() {
        let mut aux = ThreadStatsAux {
            outside_sample_pending: true,
            sample_countdown: 0,
            ..ThreadStatsAux::new()
        };

        decide_operation_sampling_with_enabled(&mut aux, false);

        assert!(!aux.op_sampled);
        assert!(aux.op_sample_decided);
    }

    #[test]
    fn outside_gap_sampling_gate_blocks_samples_when_disabled() {
        let mut aux = ThreadStatsAux::new();

        assert!(!begin_outside_gap_sample_with_enabled(&mut aux, false));
        assert_eq!(aux.sample_countdown, 0);
    }

    #[test]
    fn outside_pending_selects_full_operation_sample_when_enabled() {
        let mut aux = ThreadStatsAux {
            outside_sample_pending: true,
            sample_countdown: 3,
            ..ThreadStatsAux::new()
        };

        decide_operation_sampling_with_enabled(&mut aux, true);

        assert!(aux.op_sampled);
        assert!(aux.op_sample_decided);
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
            assert_eq!((*aux).wait_total, 50 * super::DEFAULT_SAMPLE_STRIDE);
            assert_eq!((*aux).wait_sample_count, 1);
            assert_eq!((*aux).hold_total, 80 * super::DEFAULT_SAMPLE_STRIDE);
            assert_eq!((*aux).hold_sample_count, 1);
            assert_eq!((*aux).lock_count, 1);
            assert_eq!((*aux).outside_gap_total, 0);
            assert_eq!((*aux).outside_gap_samples, 0);
            assert_eq!((*aux).wait_start_sample, 0);
            assert_eq!((*aux).wait_end_sample, 0);
            assert_eq!((*aux).hold_start_sample, 0);
            assert!(!(*aux).op_sample_decided);
            assert!(!(*aux).op_sampled);
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
    fn record_lock_acquired_marks_unsampled_fast_path_critical_section() {
        crate::admission::reset_state();

        record_lock_acquired();

        assert_eq!(crate::admission::word_for_test(), 1);
    }

    #[test]
    fn dynamic_sample_averages_require_both_counts() {
        assert_eq!(
            dynamic_sample_averages(300, 3, 600, 3),
            Some((100.0, 200.0))
        );
        assert_eq!(dynamic_sample_averages(300, 0, 600, 3), None);
        assert_eq!(dynamic_sample_averages(300, 3, 600, 0), None);
    }

    #[test]
    fn dynamic_cpu_target_uses_wait_ratio_formula() {
        assert_eq!(
            dynamic_cpu_target_from_sample_averages(100.0, 0.0),
            Some(1.0)
        );
        assert_eq!(
            dynamic_cpu_target_from_sample_averages(100.0, 100.0),
            Some(2.0)
        );
        assert_eq!(
            dynamic_cpu_target_from_sample_averages(100.0, 1_000.0),
            Some(11.0)
        );
        assert_eq!(dynamic_cpu_target_from_sample_averages(0.0, 100.0), None);
    }

    #[test]
    fn ewma_uses_supplied_alpha() {
        assert_eq!(next_ewma(None, 5.0, 0.2), 5.0);
        assert_eq!(next_ewma(Some(5.0), 15.0, 0.2), 0.2 * 15.0 + 0.8 * 5.0);
        assert_eq!(next_ewma(Some(5.0), 15.0, 0.5), 0.5 * 15.0 + 0.5 * 5.0);
    }

    #[test]
    fn dynamic_cpu_target_uses_component_ewmas() {
        let mut control = WindowControlState::default();

        assert_eq!(
            dynamic_cpu_target_from_smoothed_components(&mut control, 100.0, 100.0, None),
            Some(2.0)
        );
        assert_eq!(control.critical_ewma_ns, Some(100.0));
        assert_eq!(control.outside_ewma_ns, Some(100.0));

        let target = dynamic_cpu_target_from_smoothed_components(&mut control, 300.0, 900.0, None)
            .expect("valid component samples should produce a target");
        let expected_critical = DYNAMIC_CPU_CRITICAL_EWMA_ALPHA * 300.0
            + (1.0 - DYNAMIC_CPU_CRITICAL_EWMA_ALPHA) * 100.0;
        let expected_outside =
            DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA * 900.0 + (1.0 - DYNAMIC_CPU_OUTSIDE_EWMA_ALPHA) * 100.0;

        assert_eq!(control.critical_ewma_ns, Some(expected_critical));
        assert_eq!(control.outside_ewma_ns, Some(expected_outside));
        assert!(
            (target - (1.0 + expected_outside / expected_critical)).abs() < f64::EPSILON,
            "target should be derived from smoothed components"
        );
    }

    #[test]
    fn dynamic_cpu_component_ewma_clamps_outlier_targets() {
        let mut control = WindowControlState::default();

        assert_eq!(
            dynamic_cpu_target_from_smoothed_components(&mut control, 300.0, 3_000.0, Some(16)),
            Some(11.0)
        );

        let target =
            dynamic_cpu_target_from_smoothed_components(&mut control, 1.0, 10_000_000.0, Some(16))
                .expect("clamped component samples should produce a target");

        assert!(target <= 16.0);
        assert!(
            control.outside_ewma_ns.unwrap() <= control.critical_ewma_ns.unwrap() * 15.0,
            "stored outside EWMA should not keep an unbounded outlier"
        );
    }

    #[test]
    fn dynamic_cpu_target_limit_is_based_on_initial_cpu_count() {
        let mut control = WindowControlState::default();

        assert_eq!(dynamic_cpu_target_limit(&mut control, Some(2)), Some(16));
        assert_eq!(dynamic_cpu_target_limit(&mut control, Some(32)), Some(16));
    }

    #[test]
    fn dynamic_cpu_dead_zone_allows_one_cpu_of_slack() {
        assert!(dynamic_cpu_count_within_dead_zone(Some(8), 7));
        assert!(dynamic_cpu_count_within_dead_zone(Some(8), 8));
        assert!(dynamic_cpu_count_within_dead_zone(Some(8), 9));
        assert!(!dynamic_cpu_count_within_dead_zone(Some(8), 6));
        assert!(!dynamic_cpu_count_within_dead_zone(Some(8), 10));
        assert!(!dynamic_cpu_count_within_dead_zone(None, 8));
    }

    #[test]
    fn dynamic_cpu_count_rounds_formula_target() {
        assert_eq!(dynamic_cpu_count_from_target(1.0), 1);
        assert_eq!(dynamic_cpu_count_from_target(1.49), 1);
        assert_eq!(dynamic_cpu_count_from_target(1.50), 2);
    }
}
