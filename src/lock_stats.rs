use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::env;
use std::fs::{File, create_dir_all};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::arch::{
    wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns, wait_time_total_to_ns,
};

const HEATMAP_PATH_ENV_KEYS: [&str; 2] = ["LOCK_STATS_HEATMAP_PATH", "LB_SIMPLE_HEATMAP_PATH"];
const HEATMAP_SAMPLE_STRIDE_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_HEATMAP_SAMPLE_STRIDE",
    "LB_SIMPLE_HEATMAP_SAMPLE_STRIDE",
];
const HEATMAP_WINDOW_NS_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_HEATMAP_WINDOW_NS",
    "LB_SIMPLE_HEATMAP_WINDOW_NS",
];
const HEATMAP_WINDOW_SAMPLES_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_HEATMAP_WINDOW_SAMPLES",
    "LB_SIMPLE_HEATMAP_WINDOW_SAMPLES",
];
const HEATMAP_MIN_WINDOW_SAMPLES_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_HEATMAP_MIN_WINDOW_SAMPLES",
    "LB_SIMPLE_HEATMAP_MIN_WINDOW_SAMPLES",
];
const HEATMAP_MAX_NS_ENV_KEYS: [&str; 3] = [
    "LOCK_STATS_HEATMAP_MAX_BIN_NS",
    "LOCK_STATS_HEATMAP_MAX_NS",
    "LB_SIMPLE_HEATMAP_MAX_BIN_NS",
];
const TIMING_SAMPLE_STRIDE_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_TIMING_SAMPLE_STRIDE",
    "LB_SIMPLE_TIMING_SAMPLE_STRIDE",
];
const OUTSIDE_SAMPLE_STRIDE_ENV_KEYS: [&str; 2] = [
    "LOCK_STATS_OUTSIDE_SAMPLE_STRIDE",
    "LB_SIMPLE_OUTSIDE_SAMPLE_STRIDE",
];
const DEFAULT_HEATMAP_SAMPLE_STRIDE: u64 = 64;
const DEFAULT_HEATMAP_WINDOW_NS: u64 = 1_000_000;
const DEFAULT_HEATMAP_MIN_WINDOW_SAMPLES: u64 = 1;
const DEFAULT_HEATMAP_MAX_NS: u64 = 1 << 20;
const DEFAULT_TIMING_SAMPLE_STRIDE: u64 = 8;
const HEATMAP_WINDOW_EWMA_ALPHA: f64 = 0.2;

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
    timing_sample_countdown: u64,
    op_sample_decided: bool,
    op_sampled: bool,
    op_timing_sampled: bool,
    outside_sample_countdown: u64,
    outside_sample_pending: bool,
    outside_unlock_sample: u64,
    outside_wait_start_sample: u64,
    outside_wait_total: u64,
    heatmap_sample_countdown: u64,
    heatmap_sample_pending: bool,
    heatmap_pending_outside_total: u64,
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
            timing_sample_countdown: 0,
            op_sample_decided: false,
            op_sampled: false,
            op_timing_sampled: false,
            outside_sample_countdown: 0,
            outside_sample_pending: false,
            outside_unlock_sample: 0,
            outside_wait_start_sample: 0,
            outside_wait_total: 0,
            heatmap_sample_countdown: 0,
            heatmap_sample_pending: false,
            heatmap_pending_outside_total: 0,
        }
    }
}

#[derive(Clone)]
struct HeatmapConfig {
    path: PathBuf,
    sample_stride: u64,
    window_ns: u64,
    min_window_samples: u64,
    max_ns: u64,
    max_bin_index: usize,
}

impl HeatmapConfig {
    fn new(
        path: PathBuf,
        sample_stride: u64,
        window_ns: u64,
        min_window_samples: u64,
        max_ns: u64,
    ) -> Self {
        let max_ns = max_ns.max(1);
        Self {
            path,
            sample_stride: sample_stride.max(1),
            window_ns: window_ns.max(1),
            min_window_samples: min_window_samples.max(1),
            max_ns,
            max_bin_index: raw_heatmap_bin_index(max_ns),
        }
    }
}

struct HeatmapState {
    config: HeatmapConfig,
    base_window_ns: AtomicU64,
    windows: Mutex<BTreeMap<(u64, usize, usize), u64>>,
}

struct HeatmapWriteSummary {
    total_windows: usize,
    valid_windows: usize,
    invalid_windows: usize,
}

#[derive(Clone, Copy, Default)]
struct WindowSeriesStats {
    sample_count: u64,
    critical_sum_est: f64,
    outside_sum_est: f64,
    critical_mean_est: f64,
    outside_mean_est: f64,
    critical_mean_ewma: f64,
    outside_mean_ewma: f64,
}

impl HeatmapState {
    fn new(config: HeatmapConfig) -> Self {
        Self {
            config,
            base_window_ns: AtomicU64::new(0),
            windows: Mutex::new(BTreeMap::new()),
        }
    }

    #[inline(always)]
    fn record_sample(&self, sample_time_ns: u64, critical_ns: u64, outside_ns: u64) {
        let mut base_window_ns = self.base_window_ns.load(Ordering::Relaxed);
        if base_window_ns == 0 {
            match self.base_window_ns.compare_exchange(
                0,
                sample_time_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => base_window_ns = sample_time_ns,
                Err(existing) => base_window_ns = existing,
            }
        }

        let window_index = sample_time_ns.saturating_sub(base_window_ns) / self.config.window_ns;
        let critical_bin = heatmap_bin_index(critical_ns, self.config.max_bin_index);
        let outside_bin = heatmap_bin_index(outside_ns, self.config.max_bin_index);
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *windows
            .entry((window_index, critical_bin, outside_bin))
            .or_insert(0) += 1;
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

fn first_env_string(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn parse_positive_u64(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().filter(|value| *value > 0)
}

fn parse_env_u64(keys: &[&str], default: u64) -> u64 {
    first_env_string(keys)
        .as_deref()
        .and_then(parse_positive_u64)
        .unwrap_or(default)
}

fn warn_ignored_env(keys: &[&str], replacement_keys: &[&str]) {
    for key in keys {
        let Ok(value) = env::var(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        eprintln!(
            "[lock_stats] ignoring {}={}; use {} instead",
            key,
            value,
            replacement_keys.join(" or ")
        );
        break;
    }
}

fn parse_timing_sample_stride(value: Option<&str>) -> u64 {
    value
        .and_then(parse_positive_u64)
        .unwrap_or(DEFAULT_TIMING_SAMPLE_STRIDE)
}

#[inline(always)]
fn timing_sample_stride() -> u64 {
    static TIMING_SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *TIMING_SAMPLE_STRIDE.get_or_init(|| {
        parse_timing_sample_stride(first_env_string(&TIMING_SAMPLE_STRIDE_ENV_KEYS).as_deref())
    })
}

#[inline(always)]
fn outside_sample_stride() -> u64 {
    static OUTSIDE_SAMPLE_STRIDE: OnceLock<u64> = OnceLock::new();

    *OUTSIDE_SAMPLE_STRIDE.get_or_init(|| {
        first_env_string(&OUTSIDE_SAMPLE_STRIDE_ENV_KEYS)
            .as_deref()
            .and_then(parse_positive_u64)
            .unwrap_or_else(timing_sample_stride)
    })
}

#[inline(always)]
fn sampled_heatmap_stride(heatmap_stride: u64, outside_stride: u64) -> u64 {
    let heatmap_stride = heatmap_stride.max(1);
    let outside_stride = outside_stride.max(1);
    heatmap_stride.saturating_add(outside_stride - 1) / outside_stride
}

fn heatmap_state() -> Option<&'static HeatmapState> {
    static HEATMAP_STATE: OnceLock<Option<HeatmapState>> = OnceLock::new();

    HEATMAP_STATE
        .get_or_init(|| {
            let path = first_env_string(&HEATMAP_PATH_ENV_KEYS)?;
            let sample_stride = parse_env_u64(
                &HEATMAP_SAMPLE_STRIDE_ENV_KEYS,
                DEFAULT_HEATMAP_SAMPLE_STRIDE,
            );
            warn_ignored_env(
                &HEATMAP_WINDOW_SAMPLES_ENV_KEYS,
                &HEATMAP_WINDOW_NS_ENV_KEYS,
            );
            let window_ns = parse_env_u64(&HEATMAP_WINDOW_NS_ENV_KEYS, DEFAULT_HEATMAP_WINDOW_NS);
            let min_window_samples = parse_env_u64(
                &HEATMAP_MIN_WINDOW_SAMPLES_ENV_KEYS,
                DEFAULT_HEATMAP_MIN_WINDOW_SAMPLES,
            );
            let max_ns = parse_env_u64(&HEATMAP_MAX_NS_ENV_KEYS, DEFAULT_HEATMAP_MAX_NS);
            Some(HeatmapState::new(HeatmapConfig::new(
                PathBuf::from(path),
                sample_stride,
                window_ns,
                min_window_samples,
                max_ns,
            )))
        })
        .as_ref()
}

#[inline(always)]
fn raw_heatmap_bin_index(ns: u64) -> usize {
    if ns == 0 {
        0
    } else {
        (u64::BITS - ns.leading_zeros()) as usize
    }
}

#[inline(always)]
fn heatmap_bin_index(ns: u64, max_bin_index: usize) -> usize {
    raw_heatmap_bin_index(ns).min(max_bin_index)
}

#[inline(always)]
fn heatmap_bin_lower_ns(bin_index: usize) -> u64 {
    if bin_index == 0 {
        0
    } else {
        1_u64 << (bin_index - 1)
    }
}

#[inline(always)]
fn heatmap_bin_upper_ns(bin_index: usize, max_bin_index: usize) -> u64 {
    if bin_index == 0 {
        0
    } else if bin_index == max_bin_index {
        u64::MAX
    } else {
        (1_u64 << bin_index) - 1
    }
}

#[inline(always)]
fn heatmap_bin_midpoint_ns(bin_index: usize, max_bin_index: usize) -> f64 {
    let lo = heatmap_bin_lower_ns(bin_index) as f64;
    let hi = heatmap_bin_upper_ns(bin_index, max_bin_index);
    if hi == u64::MAX {
        lo
    } else {
        (lo + hi as f64) * 0.5
    }
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
    if aux.op_sample_decided {
        return;
    }

    let timing_sampled =
        advance_periodic_sample(&mut aux.timing_sample_countdown, timing_sample_stride());
    aux.op_timing_sampled = timing_sampled;
    aux.op_sampled = timing_sampled || aux.outside_sample_pending;
    aux.op_sample_decided = true;
}

#[inline(always)]
fn finish_operation_sampling(aux: &mut ThreadStatsAux) {
    aux.op_sample_decided = false;
    aux.op_sampled = false;
    aux.op_timing_sampled = false;
}

#[inline(always)]
fn begin_outside_gap_sample(aux: &mut ThreadStatsAux) -> bool {
    advance_periodic_sample(&mut aux.outside_sample_countdown, outside_sample_stride())
}

#[inline(always)]
fn finish_outside_gap_sample(aux: &mut ThreadStatsAux) {
    aux.outside_sample_pending = false;
    aux.outside_unlock_sample = 0;
    aux.outside_wait_start_sample = 0;
    aux.outside_wait_total = 0;
}

#[inline(always)]
fn next_heatmap_sample(aux: &mut ThreadStatsAux, heatmap_stride: u64) -> bool {
    let sampled_stride = sampled_heatmap_stride(heatmap_stride, outside_sample_stride());
    advance_periodic_sample(&mut aux.heatmap_sample_countdown, sampled_stride)
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

fn write_heatmap_csv(label: &str, state: &HeatmapState) -> io::Result<HeatmapWriteSummary> {
    if let Some(parent) = state.config.path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all(parent)?;
        }
    }

    let file = File::create(&state.config.path)?;
    let mut writer = BufWriter::new(file);
    let summary = write_heatmap_csv_to_writer(&mut writer, label, state)?;
    writer.flush()?;
    Ok(summary)
}

fn write_heatmap_csv_to_writer<W: Write>(
    writer: &mut W,
    label: &str,
    state: &HeatmapState,
) -> io::Result<HeatmapWriteSummary> {
    writeln!(
        writer,
        "stats_label,window_index,window_start_ns,window_end_ns,window_sample_count,window_valid,window_avg_critical_ns_est,window_avg_outside_ns_est,window_avg_critical_ns_ewma,window_avg_outside_ns_ewma,critical_bin_index,critical_bin_lo_ns,critical_bin_hi_ns,outside_bin_index,outside_bin_lo_ns,outside_bin_hi_ns,count"
    )?;

    let snapshot = {
        let windows = state
            .windows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        windows
            .iter()
            .map(|(&(window_index, critical_bin, outside_bin), &count)| {
                (window_index, critical_bin, outside_bin, count)
            })
            .collect::<Vec<_>>()
    };
    let base_window_ns = state.base_window_ns.load(Ordering::Relaxed);
    let mut window_series_stats = BTreeMap::<u64, WindowSeriesStats>::new();
    for (window_index, critical_bin, outside_bin, count) in &snapshot {
        let entry = window_series_stats.entry(*window_index).or_default();
        let count_f64 = *count as f64;
        entry.sample_count += *count;
        entry.critical_sum_est +=
            heatmap_bin_midpoint_ns(*critical_bin, state.config.max_bin_index) * count_f64;
        entry.outside_sum_est +=
            heatmap_bin_midpoint_ns(*outside_bin, state.config.max_bin_index) * count_f64;
    }
    let mut prev_critical_ewma = None;
    let mut prev_outside_ewma = None;
    for stats in window_series_stats.values_mut() {
        let sample_count_f64 = stats.sample_count.max(1) as f64;
        stats.critical_mean_est = stats.critical_sum_est / sample_count_f64;
        stats.outside_mean_est = stats.outside_sum_est / sample_count_f64;
        stats.critical_mean_ewma = match prev_critical_ewma {
            Some(prev) => {
                HEATMAP_WINDOW_EWMA_ALPHA * stats.critical_mean_est
                    + (1.0 - HEATMAP_WINDOW_EWMA_ALPHA) * prev
            }
            None => stats.critical_mean_est,
        };
        stats.outside_mean_ewma = match prev_outside_ewma {
            Some(prev) => {
                HEATMAP_WINDOW_EWMA_ALPHA * stats.outside_mean_est
                    + (1.0 - HEATMAP_WINDOW_EWMA_ALPHA) * prev
            }
            None => stats.outside_mean_est,
        };
        prev_critical_ewma = Some(stats.critical_mean_ewma);
        prev_outside_ewma = Some(stats.outside_mean_ewma);
    }
    let valid_windows = window_series_stats
        .values()
        .filter(|stats| stats.sample_count >= state.config.min_window_samples)
        .count();
    let total_windows = window_series_stats.len();

    for (window_index, critical_bin, outside_bin, count) in snapshot {
        let window_series = window_series_stats[&window_index];
        let window_sample_count = window_series.sample_count;
        let window_valid = u8::from(window_sample_count >= state.config.min_window_samples);
        let window_start_ns =
            base_window_ns.saturating_add(window_index.saturating_mul(state.config.window_ns));
        let window_end_ns = window_start_ns.saturating_add(state.config.window_ns);
        let critical_lo = heatmap_bin_lower_ns(critical_bin);
        let critical_hi = heatmap_bin_upper_ns(critical_bin, state.config.max_bin_index);
        let outside_lo = heatmap_bin_lower_ns(outside_bin);
        let outside_hi = heatmap_bin_upper_ns(outside_bin, state.config.max_bin_index);
        writeln!(
            writer,
            "{label},{window_index},{window_start_ns},{window_end_ns},{window_sample_count},{window_valid},{:.2},{:.2},{:.2},{:.2},{critical_bin},{critical_lo},{critical_hi},{outside_bin},{outside_lo},{outside_hi},{count}",
            window_series.critical_mean_est,
            window_series.outside_mean_est,
            window_series.critical_mean_ewma,
            window_series.outside_mean_ewma
        )?;
    }

    Ok(HeatmapWriteSummary {
        total_windows,
        valid_windows,
        invalid_windows: total_windows.saturating_sub(valid_windows),
    })
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

    if let Some(state) = heatmap_state() {
        match write_heatmap_csv(label, state) {
            Ok(summary) => {
                println!("heatmap_path: {}", state.config.path.display());
                println!("heatmap_sample_stride: {}", state.config.sample_stride);
                println!("heatmap_window_ns: {}", state.config.window_ns);
                println!(
                    "heatmap_min_window_samples: {}",
                    state.config.min_window_samples
                );
                println!("heatmap_total_windows: {}", summary.total_windows);
                println!("heatmap_valid_windows: {}", summary.valid_windows);
                println!("heatmap_invalid_windows: {}", summary.invalid_windows);
                println!(
                    "heatmap_window_ewma_alpha: {:.2}",
                    HEATMAP_WINDOW_EWMA_ALPHA
                );
                println!("heatmap_max_bin_ns: {}", state.config.max_ns);
            }
            Err(error) => {
                eprintln!(
                    "[lock_stats] failed to write heatmap CSV {}: {}",
                    state.config.path.display(),
                    error
                );
            }
        }
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

            if let Some(state) = heatmap_state() {
                if next_heatmap_sample(aux, state.config.sample_stride) {
                    aux.heatmap_sample_pending = true;
                    aux.heatmap_pending_outside_total = outside_total;
                } else {
                    aux.heatmap_sample_pending = false;
                    aux.heatmap_pending_outside_total = 0;
                }
            }

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
        if aux.op_timing_sampled && wait_total != 0 {
            aux.wait_total += wait_total.saturating_mul(timing_sample_stride());
            aux.wait_sample_count += 1;
        }
        aux.wait_start_sample = 0;
        aux.wait_end_sample = 0;

        let critical_total = if hold_end != 0 && aux.hold_start_sample != 0 {
            hold_end.saturating_sub(aux.hold_start_sample)
        } else {
            0
        };
        if aux.op_timing_sampled && critical_total != 0 {
            aux.hold_total += critical_total.saturating_mul(timing_sample_stride());
            aux.hold_sample_count += 1;
        }

        if aux.heatmap_sample_pending {
            if hold_end != 0 {
                if let Some(state) = heatmap_state() {
                    state.record_sample(
                        wait_time_to_ns(hold_end),
                        wait_time_elapsed_ns_between(aux.hold_start_sample, hold_end),
                        wait_time_total_to_ns(aux.heatmap_pending_outside_total),
                    );
                }
            }
            aux.heatmap_sample_pending = false;
            aux.heatmap_pending_outside_total = 0;
        }

        aux.hold_start_sample = 0;
        finish_operation_sampling(aux);

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
    use std::path::PathBuf;

    use super::{
        DEFAULT_TIMING_SAMPLE_STRIDE, HEATMAP_WINDOW_EWMA_ALPHA, HeatmapConfig, HeatmapState,
        ThreadStatsAux, advance_periodic_sample, decide_operation_sampling,
        finish_outside_gap_sample, heatmap_bin_index, heatmap_bin_lower_ns,
        heatmap_bin_midpoint_ns, heatmap_bin_upper_ns, outside_sample_stride,
        parse_timing_sample_stride, record_post_unlock, refresh_thread_elapsed_for_aux,
        sampled_heatmap_stride, write_heatmap_csv_to_writer,
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
        let mut countdown = 0;
        let mut sampled = Vec::new();

        for _ in 0..16 {
            sampled.push(advance_periodic_sample(
                &mut countdown,
                DEFAULT_TIMING_SAMPLE_STRIDE,
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
    fn parse_timing_sample_stride_uses_default_for_missing_or_invalid_values() {
        assert_eq!(
            parse_timing_sample_stride(None),
            DEFAULT_TIMING_SAMPLE_STRIDE
        );
        assert_eq!(
            parse_timing_sample_stride(Some("")),
            DEFAULT_TIMING_SAMPLE_STRIDE
        );
        assert_eq!(
            parse_timing_sample_stride(Some("0")),
            DEFAULT_TIMING_SAMPLE_STRIDE
        );
        assert_eq!(
            parse_timing_sample_stride(Some("abc")),
            DEFAULT_TIMING_SAMPLE_STRIDE
        );
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
    fn sampled_heatmap_stride_scales_with_outside_stride() {
        assert_eq!(sampled_heatmap_stride(64, 8), 8);
        assert_eq!(sampled_heatmap_stride(64, 64), 1);
        assert_eq!(sampled_heatmap_stride(65, 8), 9);
    }

    #[test]
    fn outside_pending_forces_full_sample_without_timing_weight() {
        let mut aux = ThreadStatsAux {
            outside_sample_pending: true,
            timing_sample_countdown: 3,
            ..ThreadStatsAux::new()
        };

        decide_operation_sampling(&mut aux);

        assert!(aux.op_sampled);
        assert!(!aux.op_timing_sampled);
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
                op_timing_sampled: true,
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
                op_timing_sampled: false,
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
    fn heatmap_bins_follow_power_of_two_ranges() {
        assert_eq!(heatmap_bin_index(0, 8), 0);
        assert_eq!(heatmap_bin_index(1, 8), 1);
        assert_eq!(heatmap_bin_index(2, 8), 2);
        assert_eq!(heatmap_bin_index(3, 8), 2);
        assert_eq!(heatmap_bin_index(4, 8), 3);
        assert_eq!(heatmap_bin_midpoint_ns(3, 8), 5.5);
        assert_eq!(heatmap_bin_midpoint_ns(8, 8), 128.0);
        assert_eq!(heatmap_bin_lower_ns(3), 4);
        assert_eq!(heatmap_bin_upper_ns(3, 8), 7);
    }

    #[test]
    fn heatmap_csv_writes_only_nonzero_bins() {
        let config = HeatmapConfig::new(PathBuf::from("heatmap.csv"), 1, 2, 2, 8);
        let state = HeatmapState::new(config);
        state.record_sample(100, 1, 4);
        state.record_sample(101, 1, 4);
        state.record_sample(103, 0, 0);

        let mut out = Vec::new();
        let summary = write_heatmap_csv_to_writer(&mut out, "lb_simple", &state).unwrap();
        let csv = String::from_utf8(out).unwrap();

        assert!(csv.contains(
            "stats_label,window_index,window_start_ns,window_end_ns,window_sample_count,window_valid,window_avg_critical_ns_est,window_avg_outside_ns_est,window_avg_critical_ns_ewma,window_avg_outside_ns_ewma,critical_bin_index,critical_bin_lo_ns,critical_bin_hi_ns,outside_bin_index,outside_bin_lo_ns,outside_bin_hi_ns,count"
        ));
        assert!(csv.contains("lb_simple,0,100,102,2,1,1.00,5.50,1.00,5.50,1,1,1,3,4,7,2"));
        assert!(csv.contains(&format!(
            "lb_simple,1,102,104,1,0,0.00,0.00,{:.2},{:.2},0,0,0,0,0,0,1",
            1.0 * (1.0 - HEATMAP_WINDOW_EWMA_ALPHA),
            5.5 * (1.0 - HEATMAP_WINDOW_EWMA_ALPHA),
        )));
        assert_eq!(summary.total_windows, 2);
        assert_eq!(summary.valid_windows, 1);
        assert_eq!(summary.invalid_windows, 1);
    }
}
