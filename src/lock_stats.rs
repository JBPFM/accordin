use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::env;
use std::fs::{File, create_dir_all};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::arch::{wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};

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
const DEFAULT_HEATMAP_SAMPLE_STRIDE: u64 = 64;
const DEFAULT_HEATMAP_WINDOW_NS: u64 = 1_000_000;
const DEFAULT_HEATMAP_MIN_WINDOW_SAMPLES: u64 = 1;
const DEFAULT_HEATMAP_MAX_NS: u64 = 1 << 20;
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
    pending_wait_ns: u64,
    outside_ns_gap_total: u64,
    outside_ns_gap_samples: u64,
    last_unlock_ns: u64,
    heatmap_sample_countdown: u64,
    heatmap_sample_pending: bool,
    heatmap_pending_outside_ns: u64,
}

impl ThreadStatsAux {
    const fn new() -> Self {
        Self {
            pending_wait_ns: 0,
            outside_ns_gap_total: 0,
            outside_ns_gap_samples: 0,
            last_unlock_ns: 0,
            heatmap_sample_countdown: 0,
            heatmap_sample_pending: false,
            heatmap_pending_outside_ns: 0,
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

fn parse_env_u64(keys: &[&str], default: u64) -> u64 {
    for key in keys {
        let Ok(value) = env::var(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match value.parse::<u64>() {
            Ok(parsed) if parsed > 0 => return parsed,
            Ok(_) => {
                eprintln!("[lock_stats] ignoring non-positive {}={}", key, value);
                return default;
            }
            Err(_) => {
                eprintln!("[lock_stats] ignoring invalid {}={}", key, value);
                return default;
            }
        }
    }
    default
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
fn next_heatmap_sample(aux: &mut ThreadStatsAux, sample_stride: u64) -> bool {
    if aux.heatmap_sample_countdown == 0 {
        aux.heatmap_sample_countdown = sample_stride.saturating_sub(1);
        true
    } else {
        aux.heatmap_sample_countdown -= 1;
        false
    }
}

#[inline(always)]
fn clear_pending_heatmap_sample(aux: &mut ThreadStatsAux) {
    aux.heatmap_sample_pending = false;
    aux.heatmap_pending_outside_ns = 0;
}

#[inline(always)]
fn set_pending_heatmap_sample(aux: &mut ThreadStatsAux, outside_ns: Option<u64>) {
    match outside_ns {
        Some(outside_ns) => {
            aux.heatmap_sample_pending = true;
            aux.heatmap_pending_outside_ns = outside_ns;
        }
        None => clear_pending_heatmap_sample(aux),
    }
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
        let outside_ns = take_outside_gap_sample(&mut *aux, hold_start_ns);
        if let Some(state) = heatmap_state() {
            if next_heatmap_sample(&mut *aux, state.config.sample_stride) {
                set_pending_heatmap_sample(&mut *aux, outside_ns);
            } else {
                clear_pending_heatmap_sample(&mut *aux);
            }
        }
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
        let critical_ns = hold_end_ns.saturating_sub(hold_start_ns);
        (*ctx).hold_ns_total += critical_ns;
        (*ctx).hold_start_ns = 0;
        (*ctx).lock_count += 1;
        if (*aux).heatmap_sample_pending {
            if let Some(state) = heatmap_state() {
                state.record_sample(hold_end_ns, critical_ns, (*aux).heatmap_pending_outside_ns);
            }
            clear_pending_heatmap_sample(&mut *aux);
        }
        (*aux).last_unlock_ns = hold_end_ns;
    }
}

#[inline(always)]
fn take_outside_gap_sample(aux: &mut ThreadStatsAux, hold_start_ns: u64) -> Option<u64> {
    if aux.last_unlock_ns != 0 {
        let unlock_gap_ns = hold_start_ns.saturating_sub(aux.last_unlock_ns);
        let outside_ns = unlock_gap_ns.saturating_sub(aux.pending_wait_ns);
        aux.outside_ns_gap_total += outside_ns;
        aux.outside_ns_gap_samples += 1;
        aux.pending_wait_ns = 0;
        return Some(outside_ns);
    }
    aux.pending_wait_ns = 0;
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        HEATMAP_WINDOW_EWMA_ALPHA, HeatmapConfig, HeatmapState, ThreadStatsAux, heatmap_bin_index,
        heatmap_bin_lower_ns, heatmap_bin_midpoint_ns, heatmap_bin_upper_ns, next_heatmap_sample,
        take_outside_gap_sample, write_heatmap_csv_to_writer,
    };

    #[test]
    fn outside_gap_uses_previous_unlock_and_current_wait() {
        let mut aux = ThreadStatsAux::default();

        aux.last_unlock_ns = 150;

        aux.pending_wait_ns = 20;
        let outside_ns = take_outside_gap_sample(&mut aux, 240);

        assert_eq!(outside_ns, Some(70));
        assert_eq!(aux.outside_ns_gap_total, 70);
        assert_eq!(aux.outside_ns_gap_samples, 1);
        assert_eq!(aux.pending_wait_ns, 0);
    }

    #[test]
    fn outside_gap_skips_first_acquire_without_previous_unlock() {
        let mut aux = ThreadStatsAux::default();

        aux.pending_wait_ns = 15;
        let outside_ns = take_outside_gap_sample(&mut aux, 80);

        assert_eq!(outside_ns, None);
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
            ..ThreadStatsAux::default()
        };

        let outside_ns = take_outside_gap_sample(&mut aux, 150);

        assert_eq!(outside_ns, Some(0));
        assert_eq!(aux.outside_ns_gap_total, 0);
        assert_eq!(aux.outside_ns_gap_samples, 1);
        assert_eq!(aux.pending_wait_ns, 0);
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
    fn heatmap_sampling_stride_counts_down() {
        let mut aux = ThreadStatsAux::default();

        assert!(next_heatmap_sample(&mut aux, 4));
        assert!(!next_heatmap_sample(&mut aux, 4));
        assert!(!next_heatmap_sample(&mut aux, 4));
        assert!(!next_heatmap_sample(&mut aux, 4));
        assert!(next_heatmap_sample(&mut aux, 4));
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
