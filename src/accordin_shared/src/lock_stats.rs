use std::cell::{Cell, UnsafeCell};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::admission::{self, CLASS_COUNT, UNMANAGED_LOCK_ID};
use crate::arch::{
    CacheAligned, wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns,
    wait_time_total_to_ns,
};
use crate::bpf_counters;
use crate::cpu_affinity;
use crate::mutex_hook;
use crate::width_control;

const SAMPLE_STRIDE_ENV: &str = "ACCORDIN_SAMPLE_STRIDE";

const DEFAULT_SAMPLE_STRIDE: u64 = 8;

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
    cv_sleep_total: u64,
    class_sample_armed: bool,
    class_hold_start_sample: u64,
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
            cv_sleep_total: 0,
            class_sample_armed: false,
            class_hold_start_sample: 0,
        }
    }
}

#[derive(Default)]
struct WindowControlState {
    last_dynamic_cpu_count: Option<usize>,
}

/// Per-class critical-section and non-critical-section accumulators consumed by
/// the width controller. Each class occupies its own cache line to avoid false
/// sharing between classes updated by different threads.
#[repr(C)]
struct ClassStats {
    cs_sum_ns: AtomicU64,
    cs_count: AtomicU64,
    ncs_sum_ns: AtomicU64,
    ncs_count: AtomicU64,
    acquires: AtomicU64,
}

impl ClassStats {
    const fn new() -> Self {
        Self {
            cs_sum_ns: AtomicU64::new(0),
            cs_count: AtomicU64::new(0),
            ncs_sum_ns: AtomicU64::new(0),
            ncs_count: AtomicU64::new(0),
            acquires: AtomicU64::new(0),
        }
    }
}

thread_local! {
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
    static THREAD_AUX: UnsafeCell<ThreadStatsAux> = const { UnsafeCell::new(ThreadStatsAux::new()) };
    static THREAD_LAST_UNLOCK: [Cell<u64>; CLASS_COUNT] =
        const { [const { Cell::new(0) }; CLASS_COUNT] };
    static THREAD_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

static PROCESS_THREAD_ELAPSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_WAIT_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_HOLD_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_LOCK_COUNT: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_GAP_TOTAL: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTSIDE_GAP_SAMPLES: AtomicU64 = AtomicU64::new(0);

static CLASS_STATS: [CacheAligned<ClassStats>; CLASS_COUNT] =
    [const { CacheAligned(ClassStats::new()) }; CLASS_COUNT];

/// Returns a pointer to the current thread's stats context.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[inline(always)]
fn thread_aux() -> *mut ThreadStatsAux {
    THREAD_AUX.with(|aux| aux.get())
}

/// Enters a lock scope for outermost attribution and reports whether this is the
/// outermost scope (the thread held nothing). Mirrors the depth guard used by
/// `admission::finish_lock_scope`.
#[inline(always)]
fn enter_lock_scope_depth() -> bool {
    THREAD_LOCK_DEPTH.with(|depth| {
        let held = depth.get();
        depth.set(held.saturating_add(1));
        held == 0
    })
}

/// Exits a lock scope and reports whether it was the outermost one.
#[inline(always)]
fn exit_lock_scope_depth() -> bool {
    THREAD_LOCK_DEPTH.with(|depth| {
        let held = depth.get();
        if held == 0 {
            return false;
        }
        depth.set(held - 1);
        held == 1
    })
}

#[cfg(test)]
fn last_unlock_sample(class: u32) -> u64 {
    if class == UNMANAGED_LOCK_ID {
        return 0;
    }
    THREAD_LAST_UNLOCK.with(|slots| slots.get(class as usize).map_or(0, Cell::get))
}

/// Stores the class's unlock timestamp and returns the one it replaced, so the
/// attribution path reaches thread-local storage once instead of twice.
#[inline(always)]
fn replace_last_unlock_sample(class: u32, sample: u64) -> u64 {
    if class == UNMANAGED_LOCK_ID {
        return 0;
    }
    THREAD_LAST_UNLOCK.with(|slots| {
        slots
            .get(class as usize)
            .map_or(0, |slot| slot.replace(sample))
    })
}

/// Non-critical-section gap between a class's previous unlock and its next
/// acquire, in raw timing units. `None` when the class has no prior unlock.
#[inline(always)]
fn last_unlock_gap_sample(previous_unlock: u64, acquire_sample: u64) -> Option<u64> {
    (previous_unlock != 0).then(|| acquire_sample.saturating_sub(previous_unlock))
}

#[inline(always)]
fn class_stats(class: u32) -> Option<&'static ClassStats> {
    if class == UNMANAGED_LOCK_ID {
        return None;
    }
    CLASS_STATS.get(class as usize).map(|aligned| &aligned.0)
}

/// Accumulates one per-class sample. The critical section is always present; the
/// non-critical section is contributed only when a prior unlock of the class was
/// observed. Index 0 (unmanaged) is dropped. Reports whether it was accounted.
#[inline(always)]
fn record_class_sample(class: u32, critical_ns: u64, outside_ns: Option<u64>) -> bool {
    let Some(stats) = class_stats(class) else {
        return false;
    };
    stats.cs_sum_ns.fetch_add(critical_ns, Ordering::Relaxed);
    stats.cs_count.fetch_add(1, Ordering::Relaxed);
    stats.acquires.fetch_add(1, Ordering::Relaxed);
    if let Some(outside_ns) = outside_ns {
        stats.ncs_sum_ns.fetch_add(outside_ns, Ordering::Relaxed);
        stats.ncs_count.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Monotonic count of samples attributed to a class. Drives the controller's
/// full-window gate and survives `take_class_sample_averages`.
pub fn class_acquires(class: u32) -> u64 {
    class_stats(class).map_or(0, |stats| stats.acquires.load(Ordering::Relaxed))
}

/// Swaps out and averages one class's critical and non-critical accumulators.
/// The monotonic `acquires` counter is preserved for exit reporting. Consumed by
/// the width controller.
pub fn take_class_sample_averages(class: u32) -> Option<(f64, f64)> {
    let stats = class_stats(class)?;
    let cs_count = stats.cs_count.swap(0, Ordering::Relaxed);
    let ncs_count = stats.ncs_count.swap(0, Ordering::Relaxed);
    let cs_sum_ns = stats.cs_sum_ns.swap(0, Ordering::Relaxed);
    let ncs_sum_ns = stats.ncs_sum_ns.swap(0, Ordering::Relaxed);
    class_sample_averages(cs_sum_ns, cs_count, ncs_sum_ns, ncs_count)
}

fn class_sample_averages(
    cs_sum_ns: u64,
    cs_count: u64,
    ncs_sum_ns: u64,
    ncs_count: u64,
) -> Option<(f64, f64)> {
    if cs_count == 0 || ncs_count == 0 {
        return None;
    }

    Some((
        cs_sum_ns as f64 / cs_count as f64,
        ncs_sum_ns as f64 / ncs_count as f64,
    ))
}

/// Attributes the outermost critical section and its per-lock non-critical
/// section to `class`, then refreshes the class's last-unlock timestamp so the
/// next acquire can measure its gap. Reports whether a sample was recorded.
/// Only invoked on outermost unlocks.
///
/// The non-critical section must exclude queueing, otherwise the
/// `1 + ncs / cs` model measures the very contention it is meant to size.
/// Two spans of the gap are queueing rather than think time and are removed
/// from it: `wait_sample`, the time this acquire spent blocked on the lock
/// itself, and `cv_sleep_sample`, the time spent parked in a condition-variable
/// wait between the class's previous unlock and this acquire. Both are disjoint
/// and both lie inside the gap. A condition variable serializes its waiters, so
/// leaving its sleep in would let a waiting turn read as concurrency the class
/// can absorb. Time spent under other locks stays in the gap and biases the
/// estimate high, which the occupancy feedback and the CPU budget then trim.
#[inline(always)]
fn attribute_outermost_class_sample(
    aux: &mut ThreadStatsAux,
    class: u32,
    unlock_sample: u64,
    wait_sample: u64,
    cv_sleep_sample: u64,
) -> bool {
    let previous_unlock = replace_last_unlock_sample(class, unlock_sample);
    if !aux.class_sample_armed {
        return false;
    }

    let critical_ns = wait_time_elapsed_ns_between(aux.class_hold_start_sample, unlock_sample);
    let outside_ns =
        last_unlock_gap_sample(previous_unlock, aux.class_hold_start_sample).map(|gap| {
            wait_time_total_to_ns(
                gap.saturating_sub(wait_sample)
                    .saturating_sub(cv_sleep_sample),
            )
        });
    let recorded = record_class_sample(class, critical_ns, outside_ns);
    aux.class_sample_armed = false;
    aux.class_hold_start_sample = 0;
    recorded
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

/// Per-class sampling exists to feed the width controller, so the controller's
/// master switch is its only source: with the feature off nothing is measured
/// and no tick runs.
#[inline(always)]
fn sampling_enabled() -> bool {
    static SAMPLING_ENABLED: OnceLock<bool> = OnceLock::new();

    *SAMPLING_ENABLED.get_or_init(width_control::enabled)
}

#[inline(always)]
fn sample_stride() -> u64 {
    static SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *SAMPLE_STRIDE
        .get_or_init(|| parse_sample_stride(first_env_string(SAMPLE_STRIDE_ENV).as_deref()))
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
    THREAD_LOCK_DEPTH.with(|depth| depth.set(0));
    THREAD_LAST_UNLOCK.with(|slots| {
        for slot in slots.iter() {
            slot.set(0);
        }
    });
    admission::reset_transient_state();
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

    print_cv_admission_counters();
    print_bpf_routing_counters();

    if let Some(cpu_count) = cpu_affinity::current_dynamic_cpu_count() {
        println!("dynamic_cpu_affinity_cpus: {cpu_count}");
    }

    for (class, aligned) in CLASS_STATS.iter().enumerate().skip(1) {
        let stats = &aligned.0;
        let acquires = stats.acquires.load(Ordering::Relaxed);
        if acquires == 0 {
            continue;
        }
        let cs_count = stats.cs_count.load(Ordering::Relaxed);
        let ncs_count = stats.ncs_count.load(Ordering::Relaxed);
        let cs_sum_ns = stats.cs_sum_ns.load(Ordering::Relaxed);
        let ncs_sum_ns = stats.ncs_sum_ns.load(Ordering::Relaxed);
        let avg_cs_ns = if cs_count != 0 {
            cs_sum_ns as f64 / cs_count as f64
        } else {
            0.0
        };
        let avg_ncs_ns = if ncs_count != 0 {
            ncs_sum_ns as f64 / ncs_count as f64
        } else {
            0.0
        };
        // The controller drains the accumulators every tick, so avg_cs_ns and
        // avg_ncs_ns cover only the residual window; its EWMAs carry the signal
        // the widths were decided from.
        let mut line = format!(
            "class_stats: id={class} avg_cs_ns={avg_cs_ns:.2} avg_ncs_ns={avg_ncs_ns:.2} acquires={acquires}"
        );
        if let Some(report) = width_control::class_report(class as u32) {
            let history = report
                .history
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            line.push_str(&format!(
                " width={} published_width={} seed={} width_changes={} ewma_cs_ns={:.2} ewma_ncs_ns={:.2} width_history={history}",
                report.width,
                report.published,
                report.seed,
                report.changes,
                report.cs_ewma_ns,
                report.ncs_ewma_ns,
            ));
        }
        println!("{line}");
    }

    admission::dump_dependency_diagnostics();
}

/// Both groups print unconditionally once their gate is on, so a missing line
/// means the evidence was disabled rather than observed as zero.
fn print_cv_admission_counters() {
    if !mutex_hook::cv_admission_counters_enabled() {
        return;
    }

    let counters = mutex_hook::cv_admission_counters();
    eprintln!(
        "[lock_stats] cv_admission hints_published={} specialized_relocks={} fallback_relocks={} route_relocks={}",
        counters.hints_published,
        counters.specialized_relocks,
        counters.fallback_relocks,
        counters.route_relocks
    );
}

fn print_bpf_routing_counters() {
    let Some(values) = bpf_counters::routing_counters() else {
        return;
    };

    let fields = bpf_counters::ROUTING_COUNTER_NAMES
        .iter()
        .zip(values.iter())
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("[lock_stats] bpf_routing {fields}");
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
fn add_cv_sleep_span(aux: &mut ThreadStatsAux, sleep_start: u64, sleep_end: u64) {
    aux.cv_sleep_total = aux
        .cv_sleep_total
        .saturating_add(sleep_end.saturating_sub(sleep_start));
}

/// Opens the bracket around a condition-variable sleep, which the cond-wait
/// hook enters after releasing the mutex and leaves before re-acquiring it.
///
/// The sleep gets its own accumulator rather than reusing
/// `wait_start_sample`/`wait_end_sample`: those hold a single open interval that
/// the re-acquisition following the sleep overwrites, and the totals they feed
/// report time blocked on a mutex, which a parked waiter is not. Returns 0 when
/// the operation is not sampled, which `record_cv_sleep_end` reads as no
/// interval. The sampling decision is the same one the following acquire would
/// reach: it is already fixed for this operation by the unlock that preceded the
/// sleep.
#[inline(always)]
pub fn record_cv_sleep_start() -> u64 {
    unsafe {
        let aux = &mut *thread_aux();
        decide_operation_sampling(aux);
        if !aux.op_sampled {
            return 0;
        }
    }

    wait_time_start()
}

/// Closes the bracket and adds the sleep to the span that the next outermost
/// unlock removes from its class's non-critical section.
#[inline(always)]
pub fn record_cv_sleep_end(sleep_start: u64) {
    if sleep_start == 0 {
        return;
    }

    let sleep_end = wait_time_start();
    unsafe {
        add_cv_sleep_span(&mut *thread_aux(), sleep_start, sleep_end);
    }
}

#[inline(always)]
pub fn record_lock_acquired() {
    admission::mark_critical_section_entered();
    record_lock_acquired_stats();
}

#[inline(always)]
pub fn record_lock_acquired_for_lock(lock_id: u32) {
    let scope = admission::begin_lock_scope(lock_id);
    record_lock_acquired_for_scope(scope);
}

#[inline(always)]
pub fn record_lock_acquired_for_scope(scope: admission::LockAdmissionScope) {
    admission::mark_critical_section_entered_for_scope(scope);
    record_lock_acquired_stats();
}

#[inline(always)]
fn record_lock_acquired_stats() {
    let enabled = sampling_enabled();
    let outermost = enabled && enter_lock_scope_depth();

    unsafe {
        let aux = &mut *thread_aux();
        decide_operation_sampling_with_enabled(aux, enabled);
        if !aux.op_sampled {
            return;
        }
    }

    let hold_start = wait_time_start();
    unsafe {
        let aux = &mut *thread_aux();
        ensure_thread_start_sample(aux, hold_start);
        aux.hold_start_sample = hold_start;

        // Attribute the critical section and its per-lock non-critical section to
        // the outermost scope only; nested inner acquisitions fold into it.
        if outermost {
            aux.class_hold_start_sample = hold_start;
            aux.class_sample_armed = true;
        }

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

/// Closes out one unlock. Takes the raw lock id rather than its class so the
/// merge lookup happens only on the outermost unlock that actually attributes a
/// sample, and so no lock backend has to remember to map the id itself.
#[inline(always)]
pub fn record_post_unlock(hold_end: u64, lock_id: u32) {
    let enabled = sampling_enabled();
    let outermost = enabled && exit_lock_scope_depth();
    let mut tick_sample = 0;

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

        // Taken and cleared on every unlock, like the wait bracket above, so a
        // sleep only ever reaches the acquire that directly followed it: a
        // condition-variable wait taken under an outer lock is time that outer
        // critical section really spent holding, and must not be carved out of
        // the outer class's non-critical section.
        let cv_sleep_total = aux.cv_sleep_total;
        aux.cv_sleep_total = 0;

        let critical_total = if hold_end != 0 && aux.hold_start_sample != 0 {
            hold_end.saturating_sub(aux.hold_start_sample)
        } else {
            0
        };
        if aux.op_sampled && critical_total != 0 {
            aux.hold_total += critical_total.saturating_mul(sample_stride());
            aux.hold_sample_count += 1;
        }

        aux.hold_start_sample = 0;
        finish_operation_sampling(aux);

        let begin_next_sample =
            !aux.outside_sample_pending && begin_outside_gap_sample_with_enabled(aux, enabled);
        let post_unlock_sample = if begin_next_sample || (enabled && hold_end == 0) {
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

        if outermost
            && attribute_outermost_class_sample(
                aux,
                admission::class_of(lock_id),
                post_unlock_sample,
                wait_total,
                cv_sleep_total,
            )
        {
            tick_sample = post_unlock_sample;
        }
    }

    // The controller rides the class-sampling path: a tick only ever fires on
    // an unlock that just contributed fresh per-class evidence.
    if tick_sample != 0 {
        width_control::maybe_run_tick(tick_sample);
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
    use std::sync::atomic::Ordering;

    use crate::arch::wait_time_total_to_ns;

    use super::{
        DEFAULT_SAMPLE_STRIDE, THREAD_LAST_UNLOCK, THREAD_LOCK_DEPTH, ThreadStatsAux,
        add_cv_sleep_span, advance_periodic_sample, attribute_outermost_class_sample,
        begin_outside_gap_sample_with_enabled, class_sample_averages, class_stats,
        decide_operation_sampling_with_enabled, enter_lock_scope_depth, exit_lock_scope_depth,
        finish_outside_gap_sample, last_unlock_gap_sample, last_unlock_sample, parse_sample_stride,
        record_class_sample, record_cv_sleep_end, record_cv_sleep_start, record_lock_acquired,
        record_post_unlock, refresh_thread_elapsed_for_aux, replace_last_unlock_sample,
        take_class_sample_averages,
    };

    fn reset_class_stats(class: u32) {
        if let Some(stats) = class_stats(class) {
            stats.cs_sum_ns.store(0, Ordering::Relaxed);
            stats.cs_count.store(0, Ordering::Relaxed);
            stats.ncs_sum_ns.store(0, Ordering::Relaxed);
            stats.ncs_count.store(0, Ordering::Relaxed);
            stats.acquires.store(0, Ordering::Relaxed);
        }
    }

    fn reset_last_unlock() {
        THREAD_LAST_UNLOCK.with(|slots| {
            for slot in slots.iter() {
                slot.set(0);
            }
        });
    }

    fn reset_lock_scope_depth() {
        THREAD_LOCK_DEPTH.with(|depth| depth.set(0));
    }

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

        record_post_unlock(260, 0);

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

        record_post_unlock(0, 0);

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
    fn class_sample_averages_require_both_counts() {
        assert_eq!(class_sample_averages(300, 3, 600, 3), Some((100.0, 200.0)));
        assert_eq!(class_sample_averages(300, 0, 600, 3), None);
        assert_eq!(class_sample_averages(300, 3, 600, 0), None);
    }

    #[test]
    fn class_samples_accumulate_independently() {
        reset_class_stats(1);
        reset_class_stats(2);

        record_class_sample(1, 100, Some(400));
        record_class_sample(1, 300, Some(600));
        record_class_sample(2, 50, Some(50));

        assert_eq!(take_class_sample_averages(1), Some((200.0, 500.0)));
        assert_eq!(take_class_sample_averages(2), Some((50.0, 50.0)));
    }

    #[test]
    fn unmanaged_class_sample_is_dropped() {
        record_class_sample(0, 100, Some(200));

        assert!(class_stats(0).is_none());
        assert_eq!(take_class_sample_averages(0), None);
    }

    #[test]
    fn class_sample_records_cs_always_and_ncs_only_when_present() {
        reset_class_stats(3);

        record_class_sample(3, 200, Some(300));
        record_class_sample(3, 250, None);

        let stats = class_stats(3).expect("managed class");
        assert_eq!(stats.acquires.load(Ordering::Relaxed), 2);
        assert_eq!(stats.cs_count.load(Ordering::Relaxed), 2);
        assert_eq!(stats.cs_sum_ns.load(Ordering::Relaxed), 450);
        assert_eq!(stats.ncs_count.load(Ordering::Relaxed), 1);
        assert_eq!(stats.ncs_sum_ns.load(Ordering::Relaxed), 300);
    }

    #[test]
    fn take_class_sample_averages_resets_sums_and_keeps_acquires() {
        reset_class_stats(4);

        record_class_sample(4, 100, Some(200));
        record_class_sample(4, 200, Some(400));

        assert_eq!(take_class_sample_averages(4), Some((150.0, 300.0)));
        // Second read observes reset sums and counts.
        assert_eq!(take_class_sample_averages(4), None);

        let stats = class_stats(4).expect("managed class");
        assert_eq!(stats.cs_sum_ns.load(Ordering::Relaxed), 0);
        assert_eq!(stats.cs_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.ncs_sum_ns.load(Ordering::Relaxed), 0);
        assert_eq!(stats.ncs_count.load(Ordering::Relaxed), 0);
        // acquires is monotonic for exit reporting and survives the swap.
        assert_eq!(stats.acquires.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn nested_lock_scope_depth_reports_outermost_once() {
        reset_lock_scope_depth();

        assert!(enter_lock_scope_depth(), "outer acquire is outermost");
        assert!(!enter_lock_scope_depth(), "inner acquire is not outermost");
        assert!(!exit_lock_scope_depth(), "inner unlock is not outermost");
        assert!(exit_lock_scope_depth(), "outer unlock is outermost");
        assert!(!exit_lock_scope_depth(), "underflow stays non-outermost");
    }

    #[test]
    fn per_lock_ncs_gap_is_none_without_prior_unlock() {
        assert_eq!(last_unlock_gap_sample(0, 500), None);
        assert_eq!(last_unlock_gap_sample(100, 500), Some(400));
        // Saturating: an out-of-order acquire never reports a negative gap.
        assert_eq!(last_unlock_gap_sample(600, 500), Some(0));
    }

    #[test]
    fn per_lock_ncs_uses_prior_unlock_of_same_class_only() {
        reset_last_unlock();

        // Two uses of class 5 with an interleaved use of class 6.
        replace_last_unlock_sample(5, 1_000);
        replace_last_unlock_sample(6, 5_000);

        // Class 5 re-acquire at 1_300: gap 300, unaffected by class 6.
        let gap_five = last_unlock_gap_sample(last_unlock_sample(5), 1_300);
        assert_eq!(gap_five, Some(300));

        // Class 6 re-acquire at 5_500: its own gap of 500.
        let gap_six = last_unlock_gap_sample(last_unlock_sample(6), 5_500);
        assert_eq!(gap_six, Some(500));

        // Unmanaged slot is never stored.
        replace_last_unlock_sample(0, 123);
        assert_eq!(last_unlock_sample(0), 0);
    }

    #[test]
    fn per_lock_ncs_contributes_single_sample_equal_to_injected_gap() {
        reset_last_unlock();
        reset_class_stats(7);

        // Prior unlock of class 7 at 2_000; interleaved class 8 must not corrupt it.
        replace_last_unlock_sample(7, 2_000);
        replace_last_unlock_sample(8, 9_000);

        let gap = last_unlock_gap_sample(last_unlock_sample(7), 2_450)
            .expect("prior unlock yields a gap");
        assert_eq!(gap, 450);
        record_class_sample(7, 111, Some(gap));

        let stats = class_stats(7).expect("managed class");
        assert_eq!(stats.ncs_count.load(Ordering::Relaxed), 1);
        assert_eq!(stats.ncs_sum_ns.load(Ordering::Relaxed), 450);
    }

    #[test]
    fn per_lock_ncs_excludes_time_spent_waiting_for_the_lock() {
        reset_last_unlock();
        reset_class_stats(11);

        let mut aux = ThreadStatsAux {
            class_sample_armed: true,
            class_hold_start_sample: 3_000,
            ..ThreadStatsAux::new()
        };
        replace_last_unlock_sample(11, 1_000);

        // A gap of 2_000 with 1_500 of it spent queueing leaves 500 of think time.
        assert!(attribute_outermost_class_sample(
            &mut aux, 11, 3_400, 1_500, 0
        ));

        let stats = class_stats(11).expect("managed class");
        assert_eq!(stats.ncs_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats.ncs_sum_ns.load(Ordering::Relaxed),
            wait_time_total_to_ns(500)
        );
        assert_eq!(last_unlock_sample(11), 3_400);
    }

    #[test]
    fn per_lock_ncs_excludes_a_condition_variable_sleep() {
        reset_last_unlock();
        reset_class_stats(12);
        reset_class_stats(13);

        // Both classes see the same 10_000 gap, 300 of it spent queueing for the
        // lock. Class 12 spent 9_500 of the rest parked in a condition-variable
        // wait; class 13 spent all of it outside any lock.
        let sleeper_ncs = {
            let mut aux = ThreadStatsAux {
                class_sample_armed: true,
                class_hold_start_sample: 11_000,
                ..ThreadStatsAux::new()
            };
            replace_last_unlock_sample(12, 1_000);
            assert!(attribute_outermost_class_sample(
                &mut aux, 12, 11_400, 300, 9_500
            ));
            let stats = class_stats(12).expect("managed class");
            assert_eq!(stats.ncs_count.load(Ordering::Relaxed), 1);
            stats.ncs_sum_ns.load(Ordering::Relaxed)
        };

        let thinker_ncs = {
            let mut aux = ThreadStatsAux {
                class_sample_armed: true,
                class_hold_start_sample: 11_000,
                ..ThreadStatsAux::new()
            };
            replace_last_unlock_sample(13, 1_000);
            assert!(attribute_outermost_class_sample(
                &mut aux, 13, 11_400, 300, 0
            ));
            let stats = class_stats(13).expect("managed class");
            stats.ncs_sum_ns.load(Ordering::Relaxed)
        };

        assert_eq!(sleeper_ncs, wait_time_total_to_ns(200));
        assert_eq!(thinker_ncs, wait_time_total_to_ns(9_700));
        assert!(
            sleeper_ncs < thinker_ncs,
            "the sleep must not read as think time"
        );
    }

    #[test]
    fn cv_sleep_spans_accumulate_across_repeated_waits() {
        let mut aux = ThreadStatsAux::new();

        add_cv_sleep_span(&mut aux, 1_000, 1_700);
        add_cv_sleep_span(&mut aux, 2_000, 2_300);
        assert_eq!(aux.cv_sleep_total, 1_000);

        // Saturating: an out-of-order pair never subtracts from the total.
        add_cv_sleep_span(&mut aux, 3_000, 2_500);
        assert_eq!(aux.cv_sleep_total, 1_000);
    }

    #[test]
    fn cv_sleep_bracket_is_gated_on_the_sampled_operation() {
        let aux = super::thread_aux();

        unsafe {
            *aux = ThreadStatsAux {
                op_sample_decided: true,
                op_sampled: false,
                ..ThreadStatsAux::new()
            };
        }
        assert_eq!(record_cv_sleep_start(), 0);
        record_cv_sleep_end(0);
        unsafe {
            assert_eq!((*aux).cv_sleep_total, 0);
        }

        unsafe {
            *aux = ThreadStatsAux {
                op_sample_decided: true,
                op_sampled: true,
                ..ThreadStatsAux::new()
            };
        }
        let sleep_start = record_cv_sleep_start();
        assert_ne!(sleep_start, 0, "a sampled operation opens the bracket");
        record_cv_sleep_end(sleep_start);
        unsafe {
            assert!(
                (*aux).cv_sleep_total < 1_000_000_000,
                "the closed bracket accumulates the span, not the clock origin"
            );
            *aux = ThreadStatsAux::new();
        }
    }

    #[test]
    fn post_unlock_clears_the_cv_sleep_span_it_consumed() {
        let aux = super::thread_aux();
        reset_lock_scope_depth();

        unsafe {
            *aux = ThreadStatsAux {
                thread_start_sample: 100,
                hold_start_sample: 180,
                cv_sleep_total: 4_000,
                op_sample_decided: true,
                op_sampled: true,
                ..ThreadStatsAux::new()
            };
        }

        record_post_unlock(260, 0);

        unsafe {
            assert_eq!((*aux).cv_sleep_total, 0);
            *aux = ThreadStatsAux::new();
        }
    }

    #[test]
    fn class_stats_slots_do_not_alias_across_classes() {
        reset_class_stats(9);
        reset_class_stats(10);

        record_class_sample(9, 10, Some(20));

        let other = class_stats(10).expect("managed class");
        assert_eq!(other.acquires.load(Ordering::Relaxed), 0);
        assert_eq!(other.cs_sum_ns.load(Ordering::Relaxed), 0);

        let touched = class_stats(9).expect("managed class");
        assert_eq!(touched.acquires.load(Ordering::Relaxed), 1);
    }
}
