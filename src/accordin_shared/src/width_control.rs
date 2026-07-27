// SPDX-License-Identifier: GPL-2.0-only

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::admission::{self, CLASS_COUNT, MAX_LOCK_CLASSES, UNMANAGED_LOCK_ID};
use crate::lock_stats;

const WIDTH_CONTROL_ENV: &str = "ACCORDIN_WIDTH_CONTROL";
const FIXED_WIDTH_ENV: &str = "ACCORDIN_FIXED_WIDTH";
const WIDTH_CLASS_MAP_ENV: &str = "ACCORDIN_WIDTH_CLASS_MAP";
const HOLDOFF_WINDOWS_ENV: &str = "ACCORDIN_WIDTH_HOLDOFF_WINDOWS";
const DEADZONE_ENV: &str = "ACCORDIN_WIDTH_DEADZONE";
const WIDTH_MAX_ENV: &str = "ACCORDIN_WIDTH_MAX";
const WIDTH_MIN_ENV: &str = "ACCORDIN_WIDTH_MIN";
const TICK_WINDOW_NS_ENV: &str = "ACCORDIN_DYNAMIC_CPU_WINDOW_NS";

/// Ceiling for every width-shaped value read from the environment: configured
/// class widths, the hard cap, the floor, and the dead zone. `class_width` caps
/// how many CPUs may serve a class concurrently, so no real machine needs a
/// larger value; bounding it keeps a stray env value out of the diagnostics.
/// Zero still means unlimited.
const MAX_STATIC_WIDTH: u32 = 4096;

const DEFAULT_HOLDOFF_WINDOWS: u32 = 3;
const DEFAULT_DEADZONE: u32 = 1;
const DEFAULT_TICK_WINDOW_NS: u64 = 10_000_000;

/// A class's admitted set holds its waiters only: the lock holder runs whatever
/// the width says, so a width of 1 admits one successor alongside the holder.
/// One admitted waiter is not enough to keep a class moving — a class held
/// there under high-frequency contention on a short critical section stalls
/// outright, long enough for the sched_ext watchdog to eject the scheduler — so
/// the floor is two admitted waiters, the narrowest width at which such a class
/// keeps making progress. Widths never reach 0, which BPF reads as unlimited.
/// Statically configured widths are exempt; the floor bounds what the feedback
/// loop may converge on.
const DEFAULT_MIN_WIDTH: u32 = 2;

/// The critical section is smoothed aggressively and the non-critical section
/// slowly: service time is the quantity the model divides by, so it must track
/// the workload, while the think time is the noisy term.
const CRITICAL_EWMA_ALPHA: f64 = 0.5;
const OUTSIDE_EWMA_ALPHA: f64 = 0.1;

/// Serialization cost that the measured hold time does not contain: the lock
/// line and the data it protects must migrate to the next holder before its
/// critical section can start. Charged to the modelled service time so the seed
/// does not ask for more concurrency than the handoff can sustain.
const MULTICORE_CRITICAL_PENALTY_NS: f64 = 50.0;
const CROSS_NUMA_CRITICAL_PENALTY_NS: f64 = 200.0;

/// A class must have contributed at least this many sampled acquires since its
/// last width change before the controller may move it again. Decouples the
/// adjustment from the measurement: a decision never rests on a window that the
/// class barely participated in.
const MIN_ACQUIRES_PER_DECISION: u64 = 32;

/// Recent committed widths kept per class for exit reporting; a converged
/// controller shows a short run of identical trailing values.
const WIDTH_HISTORY_LEN: usize = 8;

/// Managed class ranks, i.e. `CLASS_COUNT` without the unmanaged sentinel.
/// Matches `MANAGED_LOCK_COUNT` on the BPF side, which walks a ring of exactly
/// this many ranks.
const MANAGED_CLASS_COUNT: u32 = MAX_LOCK_CLASSES - 1;

static CLASS_WIDTH_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static CLASS_ACTIVE_PTR: AtomicPtr<i32> = AtomicPtr::new(std::ptr::null_mut());
static CLASS_ACTIVE_PEAK_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static CLASS_INACTIVE_DEPTH_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static CLASS_INACTIVE_DEPTH_PEAK_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static INACTIVE_PROBE_SPAN_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());

static LAST_TICK_NS: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::env::env_flag(WIDTH_CONTROL_ENV))
}

/// Returns statically configured per-class widths when `ACCORDIN_FIXED_WIDTH`
/// and/or `ACCORDIN_WIDTH_CLASS_MAP` request an open-loop oracle, or `None`
/// when neither is set. Index 0 is the unmanaged sentinel and stays 0.
///
/// Gated on the feature switch here rather than at the call site, so no caller
/// can apply a static width while width control is off.
pub fn fixed_class_widths() -> Option<[u32; CLASS_COUNT]> {
    if !enabled() {
        return None;
    }

    let (fixed, class_map) = fixed_width_settings();
    parse_fixed_class_widths(fixed.as_deref(), class_map.as_deref())
}

fn fixed_width_settings() -> (Option<String>, Option<String>) {
    (
        std::env::var(FIXED_WIDTH_ENV).ok(),
        std::env::var(WIDTH_CLASS_MAP_ENV).ok(),
    )
}

/// Whether raw settings ask for statically configured widths. Gated on the
/// feature switch: with width control off there is no width to fix, so neither
/// variable is read as configuration.
pub(crate) fn fixed_widths_present_with(
    enabled: bool,
    fixed: Option<&str>,
    class_map: Option<&str>,
) -> bool {
    enabled && parse_fixed_class_widths(fixed, class_map).is_some()
}

/// Whether an operator configured static widths, resolved once.
/// `fixed_class_widths` reads the environment on every call; callers on the
/// lock acquisition path must come through here instead.
pub(crate) fn fixed_widths_configured() -> bool {
    static CONFIGURED: OnceLock<bool> = OnceLock::new();
    *CONFIGURED.get_or_init(|| {
        let (fixed, class_map) = fixed_width_settings();
        fixed_widths_present_with(enabled(), fixed.as_deref(), class_map.as_deref())
    })
}

fn parse_fixed_class_widths(
    fixed: Option<&str>,
    class_map: Option<&str>,
) -> Option<[u32; CLASS_COUNT]> {
    if fixed.is_none() && class_map.is_none() {
        return None;
    }

    let mut widths = [0u32; CLASS_COUNT];

    if let Some(value) = fixed.and_then(parse_width) {
        let managed_start = UNMANAGED_LOCK_ID as usize + 1;
        for slot in &mut widths[managed_start..] {
            *slot = value;
        }
    }

    if let Some(class_map) = class_map {
        apply_class_map(&mut widths, class_map);
    }

    Some(widths)
}

fn apply_class_map(widths: &mut [u32; CLASS_COUNT], spec: &str) {
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let Some((class, width)) = entry.split_once(':') else {
            continue;
        };
        let Ok(class) = class.trim().parse::<u32>() else {
            continue;
        };
        let Some(width) = parse_width(width) else {
            continue;
        };

        if class == UNMANAGED_LOCK_ID || class >= MAX_LOCK_CLASSES {
            continue;
        }

        widths[class as usize] = width;
    }
}

fn parse_width(value: &str) -> Option<u32> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .map(|width| width.min(MAX_STATIC_WIDTH))
}

fn parse_tick_window_ns(value: Option<&str>) -> u64 {
    value
        .map(str::trim)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TICK_WINDOW_NS)
}

/// The controller is a closed loop only when the feature is on and no static
/// oracle width was requested; a fixed width runs open loop.
fn controller_active_with(enabled: bool, fixed_widths_present: bool) -> bool {
    enabled && !fixed_widths_present
}

fn controller_active() -> bool {
    static ACTIVE: OnceLock<bool> = OnceLock::new();
    *ACTIVE.get_or_init(|| controller_active_with(enabled(), fixed_widths_configured()))
}

fn tick_window_ns() -> u64 {
    static WINDOW_NS: OnceLock<u64> = OnceLock::new();
    *WINDOW_NS
        .get_or_init(|| parse_tick_window_ns(std::env::var(TICK_WINDOW_NS_ENV).ok().as_deref()))
}

fn holdoff_windows() -> u32 {
    static HOLDOFF: OnceLock<u32> = OnceLock::new();
    *HOLDOFF.get_or_init(|| {
        crate::env::env_u32_clamped(HOLDOFF_WINDOWS_ENV, DEFAULT_HOLDOFF_WINDOWS, 0, 1024)
    })
}

fn deadzone() -> u32 {
    static DEADZONE: OnceLock<u32> = OnceLock::new();
    *DEADZONE.get_or_init(|| {
        crate::env::env_u32_clamped(DEADZONE_ENV, DEFAULT_DEADZONE, 0, MAX_STATIC_WIDTH)
    })
}

/// CPUs this process may run on. Read from the affinity mask so a `taskset`ed
/// run arbitrates against the cores it actually owns.
fn cpu_budget() -> u32 {
    static BUDGET: OnceLock<u32> = OnceLock::new();
    *BUDGET.get_or_init(|| affinity_cpu_count().max(1))
}

fn width_max() -> u32 {
    static WIDTH_MAX: OnceLock<u32> = OnceLock::new();
    *WIDTH_MAX.get_or_init(|| {
        crate::env::env_u32_clamped(WIDTH_MAX_ENV, cpu_budget(), 1, MAX_STATIC_WIDTH)
    })
}

/// Narrowest width the feedback loop may publish for a class that has demand.
/// Never above the hard cap, so a one-CPU budget still resolves to 1.
fn min_width() -> u32 {
    static MIN_WIDTH: OnceLock<u32> = OnceLock::new();
    *MIN_WIDTH.get_or_init(|| {
        clamped_min_width(
            crate::env::env_u32_clamped(WIDTH_MIN_ENV, DEFAULT_MIN_WIDTH, 1, MAX_STATIC_WIDTH),
            width_max(),
        )
    })
}

fn clamped_min_width(min_width: u32, width_max: u32) -> u32 {
    min_width.clamp(1, width_max.max(1))
}

fn affinity_cpu_count() -> u32 {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return 0;
        }
        libc::CPU_COUNT(&set).max(0) as u32
    }
}

/// CPUs of the first NUMA node. `None` when the topology is not exposed.
fn first_node_cpu_count() -> Option<u32> {
    let spec = std::fs::read_to_string("/sys/devices/system/node/node0/cpulist").ok()?;
    Some(count_cpulist(&spec))
}

/// Counts the ids listed by a sysfs cpulist (`"0,2-6,8"`).
fn count_cpulist(spec: &str) -> u32 {
    let mut count = 0;
    for entry in spec.trim().split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        let (first, last) = match entry.split_once('-') {
            Some((first, last)) => (first.trim(), last.trim()),
            None => (entry, entry),
        };
        let (Ok(first), Ok(last)) = (first.parse::<u32>(), last.parse::<u32>()) else {
            continue;
        };
        if last < first {
            continue;
        }

        count += last - first + 1;
    }
    count
}

fn critical_penalty_ns() -> f64 {
    static PENALTY: OnceLock<f64> = OnceLock::new();
    *PENALTY.get_or_init(|| numa_critical_penalty_ns(cpu_budget(), first_node_cpu_count()))
}

fn numa_critical_penalty_ns(budget: u32, first_node_cpus: Option<u32>) -> f64 {
    if budget <= 1 {
        return 0.0;
    }

    match first_node_cpus {
        Some(first_node_cpus) if first_node_cpus > 0 && budget > first_node_cpus => {
            CROSS_NUMA_CRITICAL_PENALTY_NS
        }
        _ => MULTICORE_CRITICAL_PENALTY_NS,
    }
}

fn next_ewma(previous: Option<f64>, value: f64, alpha: f64) -> f64 {
    match previous {
        Some(previous) => alpha * value + (1.0 - alpha) * previous,
        None => value,
    }
}

/// `1 + think / service`: the number of threads that keeps the class busy
/// without piling up. Seeds a class and afterwards bounds which direction the
/// occupancy feedback may step; it never overwrites a committed width.
fn seed_from_averages(cs_ns: f64, ncs_ns: f64, penalty_ns: f64, width_max: u32) -> Option<u32> {
    if !cs_ns.is_finite() || !ncs_ns.is_finite() || cs_ns <= 0.0 || ncs_ns < 0.0 {
        return None;
    }

    let service_ns = cs_ns + penalty_ns;
    if service_ns <= 0.0 {
        return None;
    }

    let target = 1.0 + ncs_ns / service_ns;
    if !target.is_finite() {
        return None;
    }

    Some(target.round().clamp(1.0, width_max.max(1) as f64) as u32)
}

/// A class never needs more servers than it has holders plus waiters, whatever
/// the ratio model asks for. Per-lock think time is measured across whatever
/// else the thread did in between, so it reads high for a lock a thread rarely
/// returns to; this bound is what keeps that bias from pinning a cold class's
/// seed at the hard limit and leaving it permanently asking to grow.
fn bound_seed_by_occupancy(seed: u32, peak: u32, depth: u32) -> u32 {
    seed.min(peak.saturating_add(depth).max(1))
}

/// Keeps the model target from re-rounding across an integer boundary on
/// ordinary drift in the smoothed averages: the stored seed only moves once the
/// freshly computed one leaves its dead zone.
fn settle_seed(current: u32, fresh: u32, deadzone: u32) -> u32 {
    if current == 0 || fresh.abs_diff(current) > deadzone {
        fresh
    } else {
        current
    }
}

/// Direction the window's signals ask the width to move, in `[-1, 1]`.
///
/// `enforced` is the width BPF applied over the window the signals describe, so
/// saturation is judged against what actually limited the class. Growth needs
/// occupancy evidence *and* model headroom, which is what keeps a class with
/// more threads than cores from climbing to the budget. The dead zone is
/// two-sided around the seed: a width already within `deadzone` of the model
/// target does not move.
///
/// `peak` is admission-gated: a class is never seen holding more admissions
/// than the width that granted them, so a peak below the width is evidence of
/// spare capacity only while nothing is queued. A `depth` above zero is a
/// waiter denied admission under the current width right now, and it vetoes
/// every shrink — neither the peak nor the model may narrow a class whose
/// demand already exceeds its cap, since narrowing it lowers the ceiling the
/// next window's peak is measured against.
fn width_vote(enforced: u32, seed: u32, peak: u32, depth: u32, deadzone: u32) -> i32 {
    if enforced == 0 || seed == 0 {
        return 0;
    }

    let queued = depth > 0;

    // Under-occupied: the class never filled its width during the window and
    // held no one back. Measured inability to use the width outranks what the
    // model asks for.
    if !queued && peak.saturating_add(deadzone).saturating_add(1) <= enforced {
        return -1;
    }

    let saturated = peak >= enforced && queued;
    if saturated && seed > enforced.saturating_add(deadzone) {
        return 1;
    }

    if !queued && enforced > seed.saturating_add(deadzone) {
        -1
    } else {
        0
    }
}

/// A class may move its width only once the current one has been in force for
/// `holdoff_windows` ticks and the class has contributed a full window of
/// evidence under it.
fn step_allowed(acquires_since_change: u64, ticks_since_change: u32, holdoff_windows: u32) -> bool {
    ticks_since_change >= holdoff_windows && acquires_since_change >= MIN_ACQUIRES_PER_DECISION
}

/// Running agreement between consecutive windows.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct StepVote {
    direction: i32,
    windows: u32,
}

/// Chatter guard: a direction must repeat over `confirm_windows` consecutive
/// windows before it is applied, so one noisy window cannot move a width and a
/// signal that alternates never accumulates agreement at all.
fn confirm_step(vote: &mut StepVote, direction: i32, confirm_windows: u32) -> i32 {
    if direction == 0 {
        *vote = StepVote::default();
        return 0;
    }

    if vote.direction == direction {
        vote.windows = vote.windows.saturating_add(1);
    } else {
        *vote = StepVote {
            direction,
            windows: 1,
        };
    }

    if vote.windows >= confirm_windows.max(1) {
        *vote = StepVote::default();
        direction
    } else {
        0
    }
}

/// Confines a width to the range the controller may commit and publish. The
/// range never reaches 0, which BPF reads as unlimited, so a class the model
/// values at nothing is still published at the floor.
fn floored_width(width: u32, min_width: u32, width_max: u32) -> u32 {
    width.clamp(clamped_min_width(min_width, width_max), width_max.max(1))
}

fn stepped_width(width: u32, step: i32, min_width: u32, width_max: u32) -> u32 {
    let next = match step {
        step if step > 0 => width.saturating_add(1),
        step if step < 0 => width.saturating_sub(1),
        _ => width,
    };
    floored_width(next, min_width, width_max)
}

/// CPUs a class can actually put to work this window. A class never needs more
/// cores than it has holders plus waiters, and a window with neither is a class
/// with no demand at all: it asks for nothing so it reserves nothing of the
/// budget.
fn class_demand(width: u32, peak: u32, depth: u32, min_width: u32, width_max: u32) -> u32 {
    let occupancy = peak.saturating_add(depth);
    if occupancy == 0 {
        return 0;
    }

    floored_width(width.min(occupancy), min_width, width_max)
}

/// Splits `budget` CPUs across the classes that asked for width. A demand of 0
/// is a class with neither a holder nor a waiter this window: it is left at the
/// width it committed and takes no share, so idle classes cannot reserve the
/// budget away from the ones doing the work. A committed width of 0 is not a
/// class at all and stays 0 (unlimited).
///
/// Classes with demand keep a floor of `min_width`, which is also why the total
/// may exceed the budget when more classes are active than the budget can seat.
///
/// The demand cap only decides how scarce CPUs are shared. Without scarcity the
/// committed widths are published untouched, so the width the feedback loop
/// judges next window is the width it decided on: a quiet window must not look
/// to the loop like a narrower width that has room to grow.
fn arbitrate_budget(
    widths: &[u32; CLASS_COUNT],
    demands: &[u32; CLASS_COUNT],
    budget: u32,
    min_width: u32,
) -> [u32; CLASS_COUNT] {
    let total: u64 = demands.iter().map(|demand| u64::from(*demand)).sum();
    let budget = u64::from(budget.max(1));
    if total == 0 || total <= budget {
        return *widths;
    }

    let floor = u64::from(min_width.max(1));
    let mut shares = *widths;
    let mut granted: u64 = 0;
    for (class, demand) in demands.iter().enumerate() {
        if *demand == 0 {
            continue;
        }
        let scaled = (u64::from(*demand) * budget / total).max(floor);
        shares[class] = scaled as u32;
        granted += scaled;
    }

    // Rounding remainder goes to whoever is furthest below its demand.
    while granted < budget {
        let mut best_class = None;
        let mut best_deficit = 0;
        for (class, demand) in demands.iter().enumerate() {
            if *demand <= shares[class] {
                continue;
            }
            let deficit = *demand - shares[class];
            if deficit > best_deficit {
                best_deficit = deficit;
                best_class = Some(class);
            }
        }

        let Some(class) = best_class else {
            break;
        };
        shares[class] += 1;
        granted += 1;
    }

    shares
}

#[derive(Clone, Copy)]
struct ClassController {
    cs_ewma_ns: Option<f64>,
    ncs_ewma_ns: Option<f64>,
    seed: u32,
    /// Width the feedback loop converged on; 0 until the class is first seeded.
    width: u32,
    /// Width last published to BPF, i.e. what the next window's signals reflect.
    published: u32,
    ticks_since_change: u32,
    acquires_at_change: u64,
    vote: StepVote,
    changes: u32,
    /// Set once the class is merged away; the canonical class owns its width.
    absorbed: bool,
    history: [u32; WIDTH_HISTORY_LEN],
    history_len: usize,
    history_next: usize,
}

impl ClassController {
    const fn new() -> Self {
        Self {
            cs_ewma_ns: None,
            ncs_ewma_ns: None,
            seed: 0,
            width: 0,
            published: 0,
            ticks_since_change: 0,
            acquires_at_change: 0,
            vote: StepVote {
                direction: 0,
                windows: 0,
            },
            changes: 0,
            absorbed: false,
            history: [0; WIDTH_HISTORY_LEN],
            history_len: 0,
            history_next: 0,
        }
    }

    /// Commits the width a class enters the loop at, once the model has seeded
    /// it for the first time.
    ///
    /// That first seed describes a class nothing has bounded yet: it reports the
    /// concurrency the class reached on its own, not concurrency some enforced
    /// width denied it, so it is no evidence that a wider width is usable. A
    /// class parked at a width it cannot use stalls for every window the loop
    /// needs to step back down, so a class enters at the floor — the narrowest
    /// width at which a contended class keeps moving — and earns anything above
    /// it through the growth path, where each step needs saturation with waiters
    /// still queued behind it, confirmed over consecutive windows. The seed goes
    /// on steering that growth from the next window; it just does not decide
    /// where the class starts.
    fn commit_initial_width(&mut self, min_width: u32, width_max: u32, acquires: u64) {
        self.commit_width(clamped_min_width(min_width, width_max), acquires);
    }

    fn commit_width(&mut self, width: u32, acquires: u64) {
        self.width = width;
        self.ticks_since_change = 0;
        self.acquires_at_change = acquires;
        self.vote = StepVote::default();
        self.changes = self.changes.saturating_add(1);
        self.history[self.history_next] = width;
        self.history_next = (self.history_next + 1) % WIDTH_HISTORY_LEN;
        if self.history_len < WIDTH_HISTORY_LEN {
            self.history_len += 1;
        }
    }

    fn history(&self) -> Vec<u32> {
        let start = (self.history_next + WIDTH_HISTORY_LEN - self.history_len) % WIDTH_HISTORY_LEN;
        (0..self.history_len)
            .map(|offset| self.history[(start + offset) % WIDTH_HISTORY_LEN])
            .collect()
    }

    /// Takes over the controller state of a class merged into this one. The
    /// wider of the two widths is kept because the canonical class now serves
    /// the union of both waiter populations.
    fn absorb(&mut self, other: &ClassController, acquires: u64) {
        self.cs_ewma_ns = self.cs_ewma_ns.or(other.cs_ewma_ns);
        self.ncs_ewma_ns = self.ncs_ewma_ns.or(other.ncs_ewma_ns);
        self.seed = self.seed.max(other.seed);
        if other.width > self.width {
            self.width = other.width;
        }
        self.ticks_since_change = 0;
        self.acquires_at_change = acquires;
    }
}

type ControllerClasses = [ClassController; CLASS_COUNT];

fn controller() -> &'static Mutex<ControllerClasses> {
    static CONTROLLER: OnceLock<Mutex<ControllerClasses>> = OnceLock::new();
    CONTROLLER.get_or_init(|| Mutex::new([ClassController::new(); CLASS_COUNT]))
}

/// Folds every class that a dependency merge renamed into its canonical class.
/// The absorbed id keeps publishing 0 (unlimited): waiters parked on its DSQ
/// before the merge must stay drainable, and the authoritative cap now lives on
/// the canonical class, which every new admission word points at.
fn fold_merged_classes(
    classes: &mut ControllerClasses,
    canonical_of: impl Fn(usize) -> usize,
    acquires_of: impl Fn(usize) -> u64,
) {
    for class in 1..CLASS_COUNT {
        let canonical = canonical_of(class);
        if canonical == class || canonical == 0 || canonical >= CLASS_COUNT {
            continue;
        }
        if classes[class].absorbed {
            continue;
        }

        let absorbed = classes[class];
        classes[class] = ClassController::new();
        classes[class].absorbed = true;
        classes[canonical].absorb(&absorbed, acquires_of(canonical));
    }
}

#[inline]
fn class_ptr<T>(base: &AtomicPtr<T>, class: usize) -> Option<*mut T> {
    let base = base.load(Ordering::Acquire);
    if base.is_null() || class >= CLASS_COUNT {
        return None;
    }
    Some(unsafe { base.add(class) })
}

/// Reads and clears the window peak. BPF stores the peak without
/// synchronization, so a concurrent update may be dropped; the value is
/// advisory and the next window rebuilds it.
fn take_class_active_peak(class: usize) -> u32 {
    let Some(ptr) = class_ptr(&CLASS_ACTIVE_PEAK_PTR, class) else {
        return 0;
    };
    unsafe {
        let peak = ptr.read_volatile();
        ptr.write_volatile(0);
        peak
    }
}

/// Reads and clears the window peak of the inactive queue, on the same terms as
/// the admission peak.
fn take_class_inactive_depth_peak(class: usize) -> u32 {
    let Some(ptr) = class_ptr(&CLASS_INACTIVE_DEPTH_PEAK_PTR, class) else {
        return 0;
    };
    unsafe {
        let peak = ptr.read_volatile();
        ptr.write_volatile(0);
        peak
    }
}

/// Waiters currently parked in the class's inactive queue.
fn class_inactive_depth(class: usize) -> u32 {
    let Some(ptr) = class_ptr(&CLASS_INACTIVE_DEPTH_PTR, class) else {
        return 0;
    };
    unsafe { ptr.read_volatile() }
}

/// Waiters currently admitted for the class. A task stops counting once it
/// takes the lock, so this never includes the holder.
fn class_active(class: usize) -> u32 {
    let Some(ptr) = class_ptr(&CLASS_ACTIVE_PTR, class) else {
        return 0;
    };
    unsafe { ptr.read_volatile() }.max(0) as u32
}

/// Level a per-class signal reached over the window, from the two views BPF
/// offers of it: the high-water mark, which the controller clears by reading,
/// and the level standing at the instant of the read. A rise that fell back
/// before the tick survives only in the mark; a level still standing after an
/// earlier read consumed the mark survives only in the live view.
fn window_level(peak: u32, live: u32) -> u32 {
    peak.max(live)
}

/// How full the class got over the window. The peak only advances when a task
/// newly gains admission, so a class whose holders keep their admission across
/// many acquisitions can report a stale peak; the live level closes that gap
/// and keeps the under-occupancy rule from shrinking a saturated class.
fn class_occupancy(class: usize) -> u32 {
    window_level(take_class_active_peak(class), class_active(class))
}

/// How deep the class queued over the window. A short critical section lets a
/// contention burst park several waiters and drain them again well inside one
/// tick, so the live depth on its own reports an idle queue for a class that
/// spent the window denying admission; the peak is what carries that demand to
/// the shrink veto and the occupancy bound.
fn class_queue_depth(class: usize) -> u32 {
    window_level(
        take_class_inactive_depth_peak(class),
        class_inactive_depth(class),
    )
}

fn publish_class_width(class: usize, width: u32) {
    let Some(ptr) = class_ptr(&CLASS_WIDTH_PTR, class) else {
        return;
    };
    unsafe { ptr.write_volatile(width) }
}

/// Managed class ranks the BPF deficit probe may rotate over. Bounded by the
/// ranks that exist: the probe indexes the per-class arrays by rank, and a span
/// past the ring would send it outside them.
fn clamped_probe_span(allocated: u32) -> u32 {
    allocated.min(MANAGED_CLASS_COUNT)
}

fn store_probe_span(ptr: *mut u32, allocated: u32) {
    if ptr.is_null() {
        return;
    }
    unsafe { ptr.write_volatile(clamped_probe_span(allocated)) }
}

/// Publishes how many managed class ranks the deficit probe should rotate over.
/// Driven by lock-class allocation rather than the controller tick, which does
/// not run in fixed-width mode while the probe does.
pub fn publish_inactive_probe_span() {
    store_probe_span(
        INACTIVE_PROBE_SPAN_PTR.load(Ordering::Acquire),
        admission::allocated_class_count(),
    );
}

/// Throttled entry point driven by the per-class sampling path. Elects a single
/// writer per window by CAS so the tick body runs once and publishes once.
///
/// Takes a raw timing sample and converts it only once the controller is known
/// to be running, so an open-loop fixed-width run pays nothing for the clock
/// conversion on every sampled unlock.
#[inline]
pub fn maybe_run_tick(now_sample: u64) {
    if !controller_active() {
        return;
    }

    let now_ns = crate::arch::wait_time_to_ns(now_sample);
    let window_ns = tick_window_ns();
    let mut last_tick = LAST_TICK_NS.load(Ordering::Relaxed);

    loop {
        if last_tick == 0 {
            match LAST_TICK_NS.compare_exchange(0, now_ns, Ordering::Relaxed, Ordering::Relaxed) {
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

        match LAST_TICK_NS.compare_exchange(last_tick, now_ns, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => last_tick = actual,
        }
    }

    run_tick();
}

fn run_tick() {
    let width_max = width_max();
    let min_width = min_width();
    let budget = cpu_budget();
    let holdoff_windows = holdoff_windows();
    let deadzone = deadzone();
    let penalty_ns = critical_penalty_ns();

    let mut classes = controller()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    if admission::take_classes_dirty() {
        fold_merged_classes(
            &mut classes,
            |class| admission::class_of(class as u32) as usize,
            |class| lock_stats::class_acquires(class as u32),
        );
    }

    let mut widths = [0u32; CLASS_COUNT];
    let mut demands = [0u32; CLASS_COUNT];

    for class in 1..CLASS_COUNT {
        let peak = class_occupancy(class);
        let depth = class_queue_depth(class);
        let averages = lock_stats::take_class_sample_averages(class as u32);
        let acquires = lock_stats::class_acquires(class as u32);
        let entry = &mut classes[class];

        if entry.absorbed {
            continue;
        }

        entry.ticks_since_change = entry.ticks_since_change.saturating_add(1);

        if let Some((cs_ns, ncs_ns)) = averages {
            let cs_ewma_ns = next_ewma(entry.cs_ewma_ns, cs_ns, CRITICAL_EWMA_ALPHA);
            let ncs_ewma_ns = next_ewma(entry.ncs_ewma_ns, ncs_ns, OUTSIDE_EWMA_ALPHA);
            entry.cs_ewma_ns = Some(cs_ewma_ns);
            entry.ncs_ewma_ns = Some(ncs_ewma_ns);
            if let Some(seed) = seed_from_averages(cs_ewma_ns, ncs_ewma_ns, penalty_ns, width_max) {
                let seed = bound_seed_by_occupancy(seed, peak, depth);
                entry.seed = settle_seed(entry.seed, seed, deadzone);
            }
        }

        if entry.seed == 0 {
            // Never measured: leave the class unlimited, as it is without the
            // controller.
            continue;
        }

        if entry.width == 0 {
            entry.commit_initial_width(min_width, width_max, acquires);
        } else {
            let vote = width_vote(entry.published, entry.seed, peak, depth, deadzone);
            let step = confirm_step(&mut entry.vote, vote, holdoff_windows);
            let allowed = step_allowed(
                acquires.saturating_sub(entry.acquires_at_change),
                entry.ticks_since_change,
                holdoff_windows,
            );
            let next = stepped_width(
                entry.width,
                if allowed { step } else { 0 },
                min_width,
                width_max,
            );
            if next != entry.width {
                entry.commit_width(next, acquires);
            }
        }

        widths[class] = floored_width(entry.width, min_width, width_max);
        demands[class] = class_demand(entry.width, peak, depth, min_width, width_max);
    }

    let shares = arbitrate_budget(&widths, &demands, budget, min_width);
    for class in 1..CLASS_COUNT {
        classes[class].published = shares[class];
        publish_class_width(class, shares[class]);
    }
}

/// Per-class controller state for exit reporting.
pub struct ClassWidthReport {
    pub width: u32,
    pub published: u32,
    pub seed: u32,
    pub changes: u32,
    pub cs_ewma_ns: f64,
    pub ncs_ewma_ns: f64,
    pub history: Vec<u32>,
}

/// Final width and recent width history of a class, or `None` when the
/// controller never committed a width for it.
pub fn class_report(class: u32) -> Option<ClassWidthReport> {
    if !controller_active() || class as usize >= CLASS_COUNT {
        return None;
    }

    let classes = controller()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let entry = &classes[class as usize];
    if entry.width == 0 && entry.history_len == 0 {
        return None;
    }

    Some(ClassWidthReport {
        width: entry.width,
        published: entry.published,
        seed: entry.seed,
        changes: entry.changes,
        cs_ewma_ns: entry.cs_ewma_ns.unwrap_or(0.0),
        ncs_ewma_ns: entry.ncs_ewma_ns.unwrap_or(0.0),
        history: entry.history(),
    })
}

/// Hands BPF's per-class state to the controller. The probe span is published
/// here as well, so classes allocated before the scheduler attached are
/// covered.
#[doc(hidden)]
pub fn set_class_state_ptrs(
    class_width: *mut u32,
    class_active: *mut i32,
    class_active_peak: *mut u32,
    class_inactive_depth: *mut u32,
    class_inactive_depth_peak: *mut u32,
    inactive_probe_span: *mut u32,
) {
    CLASS_WIDTH_PTR.store(class_width, Ordering::Release);
    CLASS_ACTIVE_PTR.store(class_active, Ordering::Release);
    CLASS_ACTIVE_PEAK_PTR.store(class_active_peak, Ordering::Release);
    CLASS_INACTIVE_DEPTH_PTR.store(class_inactive_depth, Ordering::Release);
    CLASS_INACTIVE_DEPTH_PEAK_PTR.store(class_inactive_depth_peak, Ordering::Release);
    INACTIVE_PROBE_SPAN_PTR.store(inactive_probe_span, Ordering::Release);

    publish_inactive_probe_span();
}

#[doc(hidden)]
pub fn clear_class_state_ptrs() {
    set_class_state_ptrs(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        CLASS_COUNT, CROSS_NUMA_CRITICAL_PENALTY_NS, ClassController, DEFAULT_MIN_WIDTH,
        DEFAULT_TICK_WINDOW_NS, MANAGED_CLASS_COUNT, MAX_LOCK_CLASSES, MAX_STATIC_WIDTH,
        MIN_ACQUIRES_PER_DECISION, MULTICORE_CRITICAL_PENALTY_NS, StepVote, arbitrate_budget,
        bound_seed_by_occupancy, clamped_min_width, clamped_probe_span, class_demand, confirm_step,
        controller_active_with, count_cpulist, floored_width, fold_merged_classes,
        numa_critical_penalty_ns, parse_fixed_class_widths, parse_tick_window_ns,
        seed_from_averages, settle_seed, step_allowed, stepped_width, store_probe_span, width_vote,
        window_level,
    };

    const HOLDOFF: u32 = 3;
    const DEADZONE: u32 = 1;
    const ENOUGH: u64 = MIN_ACQUIRES_PER_DECISION;
    const MIN_WIDTH: u32 = DEFAULT_MIN_WIDTH;

    #[test]
    fn returns_none_when_no_static_config_present() {
        assert!(parse_fixed_class_widths(None, None).is_none());
    }

    #[test]
    fn fixed_width_applies_to_managed_classes_only() {
        let widths = parse_fixed_class_widths(Some("4"), None).expect("fixed width present");
        assert_eq!(widths[0], 0);
        for (class, width) in widths.iter().enumerate().skip(1) {
            assert_eq!(*width, 4, "class {class}");
        }
    }

    #[test]
    fn class_map_overrides_apply_on_top_of_fixed_width() {
        let widths = parse_fixed_class_widths(Some("4"), Some("1:2,2:8")).expect("config present");
        assert_eq!(widths[0], 0);
        assert_eq!(widths[1], 2);
        assert_eq!(widths[2], 8);
        assert_eq!(widths[3], 4);
    }

    #[test]
    fn class_map_alone_sets_only_named_classes() {
        let widths = parse_fixed_class_widths(None, Some("3:5")).expect("config present");
        assert_eq!(widths[3], 5);
        assert_eq!(widths[1], 0);
        assert_eq!(widths[2], 0);
    }

    #[test]
    fn invalid_entries_are_ignored_defensively() {
        let out_of_range = MAX_LOCK_CLASSES;
        let widths = parse_fixed_class_widths(
            Some("not-a-number"),
            Some(&format!("0:9,{out_of_range}:9,x:1,2:oops,3:7,,5")),
        )
        .expect("config present");
        // Unparseable fixed width leaves the all-zero base intact.
        assert_eq!(widths[1], 0);
        // The unmanaged class and a class past the limit are both rejected.
        assert_eq!(widths[0], 0);
        // Only the well-formed entry lands.
        assert_eq!(widths[3], 7);
        // Malformed entries do not set their class.
        assert_eq!(widths[2], 0);
        assert_eq!(widths[5], 0);
    }

    #[test]
    fn widths_are_clamped_to_the_static_maximum() {
        let widths = parse_fixed_class_widths(Some("100000000"), Some("1:99999999"))
            .expect("config present");
        assert_eq!(widths[1], MAX_STATIC_WIDTH);
        assert_eq!(widths[2], MAX_STATIC_WIDTH);
    }

    #[test]
    fn fixed_width_mode_runs_the_controller_open_loop() {
        assert!(controller_active_with(true, false));
        assert!(!controller_active_with(true, true));
        assert!(!controller_active_with(false, false));
        assert!(!controller_active_with(false, true));
    }

    #[test]
    fn parse_tick_window_uses_default_for_missing_or_invalid_values() {
        assert_eq!(parse_tick_window_ns(None), DEFAULT_TICK_WINDOW_NS);
        assert_eq!(parse_tick_window_ns(Some("")), DEFAULT_TICK_WINDOW_NS);
        assert_eq!(parse_tick_window_ns(Some("0")), DEFAULT_TICK_WINDOW_NS);
        assert_eq!(parse_tick_window_ns(Some("abc")), DEFAULT_TICK_WINDOW_NS);
        assert_eq!(parse_tick_window_ns(Some(" 5000 ")), 5_000);
    }

    #[test]
    fn seed_follows_one_plus_think_over_service() {
        // 1 + 3000 / 300 with no handoff penalty.
        assert_eq!(seed_from_averages(300.0, 3000.0, 0.0, 96), Some(11));
        // The penalty is charged to the service time and lowers the seed.
        assert_eq!(seed_from_averages(300.0, 3000.0, 200.0, 96), Some(7));
        // A dominant critical section asks for one server.
        assert_eq!(seed_from_averages(3000.0, 300.0, 0.0, 96), Some(1));
    }

    #[test]
    fn seed_is_clamped_and_rejects_unusable_averages() {
        assert_eq!(seed_from_averages(300.0, 3000.0, 0.0, 4), Some(4));
        assert_eq!(seed_from_averages(300.0, 0.0, 0.0, 96), Some(1));
        assert_eq!(seed_from_averages(0.0, 3000.0, 0.0, 96), None);
        assert_eq!(seed_from_averages(f64::NAN, 3000.0, 0.0, 96), None);
        assert_eq!(seed_from_averages(300.0, f64::INFINITY, 0.0, 96), None);
    }

    #[test]
    fn numa_penalty_distinguishes_single_node_from_cross_node() {
        assert_eq!(numa_critical_penalty_ns(1, Some(48)), 0.0);
        assert_eq!(
            numa_critical_penalty_ns(96, Some(48)),
            CROSS_NUMA_CRITICAL_PENALTY_NS
        );
        assert_eq!(
            numa_critical_penalty_ns(48, Some(48)),
            MULTICORE_CRITICAL_PENALTY_NS
        );
        assert_eq!(
            numa_critical_penalty_ns(96, None),
            MULTICORE_CRITICAL_PENALTY_NS
        );
    }

    #[test]
    fn cpulist_counts_single_ids_and_ranges() {
        assert_eq!(count_cpulist("0-3,8,10-11\n"), 7);
        assert_eq!(count_cpulist("0-95"), 96);
        assert_eq!(count_cpulist(" "), 0);
        // Reversed and unparseable entries are dropped.
        assert_eq!(count_cpulist("5-1,x,2"), 1);
    }

    #[test]
    fn holdoff_and_full_window_gate_both_have_to_pass() {
        assert!(step_allowed(ENOUGH, HOLDOFF, HOLDOFF));
        // The current width has not been in force for enough windows.
        assert!(!step_allowed(ENOUGH, HOLDOFF - 1, HOLDOFF));
        // The class barely participated in the windows it is judged on.
        assert!(!step_allowed(ENOUGH - 1, HOLDOFF, HOLDOFF));
    }

    #[test]
    fn a_window_keeps_whichever_of_the_mark_and_the_live_level_is_larger() {
        // A rise that fell back before the tick is only in the mark.
        assert_eq!(window_level(7, 0), 7);
        // A level that outlives the read that consumed the mark is only live.
        assert_eq!(window_level(0, 7), 7);
        // Neither view may pull the other down.
        assert_eq!(window_level(7, 3), 7);
        assert_eq!(window_level(3, 7), 7);
        // A window in which the signal never moved reports nothing.
        assert_eq!(window_level(0, 0), 0);
    }

    #[test]
    fn the_seed_never_exceeds_holders_plus_waiters() {
        // A cold class whose think time reads high is still bounded by the
        // threads that actually use it.
        assert_eq!(bound_seed_by_occupancy(96, 1, 1), 2);
        assert_eq!(bound_seed_by_occupancy(96, 9, 15), 24);
        // A busy class below its occupancy keeps its model seed.
        assert_eq!(bound_seed_by_occupancy(9, 9, 15), 9);
        // An idle window never bounds a class below one server.
        assert_eq!(bound_seed_by_occupancy(9, 0, 0), 1);
    }

    #[test]
    fn the_seed_only_moves_once_it_leaves_its_own_dead_zone() {
        // First measurement always lands.
        assert_eq!(settle_seed(0, 9, DEADZONE), 9);
        // Drift across an integer boundary is absorbed.
        assert_eq!(settle_seed(9, 10, DEADZONE), 9);
        assert_eq!(settle_seed(9, 8, DEADZONE), 9);
        // A genuine move is taken.
        assert_eq!(settle_seed(9, 11, DEADZONE), 11);
        assert_eq!(settle_seed(9, 7, DEADZONE), 7);
    }

    #[test]
    fn a_width_inside_the_dead_zone_of_the_seed_does_not_move() {
        // A saturated class one short of the seed stays put: jitter in the
        // smoothed averages must not walk the width back and forth.
        assert_eq!(width_vote(9, 10, 9, 5, DEADZONE), 0);
        assert_eq!(width_vote(9, 9, 9, 5, DEADZONE), 0);
        assert_eq!(width_vote(9, 8, 9, 5, DEADZONE), 0);
        // Outside the dead zone the width follows the model, one step at a time.
        assert_eq!(width_vote(9, 11, 9, 5, DEADZONE), 1);
        // Downwards only once the queue is empty: a saturated class with five
        // waiters holds its width whatever the seed reads.
        assert_eq!(width_vote(9, 7, 9, 5, DEADZONE), 0);
        assert_eq!(width_vote(9, 7, 8, 0, DEADZONE), -1);
    }

    #[test]
    fn growth_needs_saturation_and_model_headroom() {
        // Saturated and beyond the dead zone below the seed: grow.
        assert_eq!(width_vote(4, 8, 4, 1, DEADZONE), 1);
        // Saturated but the seed says the class is already wide enough.
        assert_eq!(width_vote(8, 8, 8, 5, DEADZONE), 0);
        // Peak reached the width but nothing is queued behind it.
        assert_eq!(width_vote(4, 8, 4, 0, DEADZONE), 0);
    }

    #[test]
    fn shrink_needs_under_occupancy_beyond_the_dead_zone() {
        // peak == width - 1 sits inside the dead zone.
        assert_eq!(width_vote(6, 8, 5, 0, DEADZONE), 0);
        // peak <= width - 1 - deadzone shrinks.
        assert_eq!(width_vote(6, 8, 4, 0, DEADZONE), -1);
        // A width the model no longer justifies shrinks too, one step at a
        // time, once nothing is queued behind it.
        assert_eq!(width_vote(8, 6, 8, 0, DEADZONE), -1);
        // ... but only outside the dead zone around the seed.
        assert_eq!(width_vote(7, 6, 7, 4, DEADZONE), 0);
        // A fully saturated class with four waiters holds: the model's lower
        // seed does not outrank real queued demand.
        assert_eq!(width_vote(8, 6, 8, 4, DEADZONE), 0);
    }

    #[test]
    fn queued_waiters_veto_a_shrink_however_the_model_and_the_peak_read() {
        // A waiter parked in the inactive queue was denied admission under the
        // width in force, so the class wants more of it than it was allowed to
        // use. Every shrink is vetoed while the queue is occupied, whether the
        // peak fell short of the width or the model asks for less than it.
        assert_eq!(width_vote(6, 3, 6, 2, DEADZONE), 0);
        assert_eq!(width_vote(6, 3, 5, 2, DEADZONE), 0);
        assert_eq!(width_vote(6, 3, 4, 2, DEADZONE), 0);
        // With the queue empty the same signals describe a class that is
        // genuinely over-provisioned, and it steps down.
        assert_eq!(width_vote(6, 3, 5, 0, DEADZONE), -1);
        assert_eq!(width_vote(6, 3, 4, 0, DEADZONE), -1);
    }

    #[test]
    fn steps_never_move_a_width_by_more_than_one() {
        assert_eq!(stepped_width(5, 1, MIN_WIDTH, 96), 6);
        assert_eq!(stepped_width(5, -1, MIN_WIDTH, 96), 4);
        assert_eq!(stepped_width(5, 0, MIN_WIDTH, 96), 5);
        // The configured hard cap.
        assert_eq!(stepped_width(4, 1, MIN_WIDTH, 4), 4);
    }

    #[test]
    fn a_width_below_the_floor_is_published_at_the_floor() {
        // A class the controller values at nothing is still published as a
        // width, because 0 reads as unlimited.
        assert_eq!(floored_width(0, MIN_WIDTH, 96), MIN_WIDTH);
        // A single server is the case the floor exists for; a raised floor
        // lifts it further.
        assert_eq!(floored_width(1, MIN_WIDTH, 96), MIN_WIDTH);
        assert_eq!(floored_width(1, 3, 96), 3);
        // Wider widths are left alone, and the hard cap still wins.
        assert_eq!(floored_width(9, MIN_WIDTH, 96), 9);
        assert_eq!(floored_width(9, MIN_WIDTH, 4), 4);
        // A floor above the cap collapses to the cap.
        assert_eq!(floored_width(3, 3, 1), 1);
    }

    #[test]
    fn a_width_never_steps_below_the_floor() {
        // Shrinking stops at the floor however hard the occupancy signal
        // pushes down.
        assert_eq!(stepped_width(MIN_WIDTH, -1, MIN_WIDTH, 96), MIN_WIDTH);
        assert_eq!(stepped_width(1, -1, MIN_WIDTH, 96), MIN_WIDTH);
        // A wider floor is honoured too.
        assert_eq!(stepped_width(3, -1, 3, 96), 3);
        // A budget that cannot seat the floor caps it instead of raising it.
        assert_eq!(stepped_width(1, -1, MIN_WIDTH, 1), 1);
        assert_eq!(clamped_min_width(MIN_WIDTH, 1), 1);
        assert_eq!(clamped_min_width(0, 96), 1);
    }

    #[test]
    fn an_unpublished_class_is_never_stepped() {
        assert_eq!(width_vote(0, 8, 30, 30, DEADZONE), 0);
    }

    #[test]
    fn a_direction_has_to_repeat_before_it_is_applied() {
        let mut vote = StepVote::default();

        assert_eq!(confirm_step(&mut vote, 1, 3), 0);
        assert_eq!(confirm_step(&mut vote, 1, 3), 0);
        assert_eq!(confirm_step(&mut vote, 1, 3), 1);
        // Applying a direction consumes the agreement.
        assert_eq!(confirm_step(&mut vote, 1, 3), 0);
    }

    #[test]
    fn an_alternating_signal_never_accumulates_agreement() {
        let mut vote = StepVote::default();

        for _ in 0..16 {
            assert_eq!(confirm_step(&mut vote, 1, 3), 0);
            assert_eq!(confirm_step(&mut vote, -1, 3), 0);
        }

        // A quiet window clears the tally outright.
        assert_eq!(confirm_step(&mut vote, 1, 3), 0);
        assert_eq!(confirm_step(&mut vote, 0, 3), 0);
        assert_eq!(vote, StepVote::default());
    }

    #[test]
    fn demand_is_capped_by_holders_plus_waiters() {
        assert_eq!(class_demand(11, 4, 2, MIN_WIDTH, 96), 6);
        assert_eq!(class_demand(11, 8, 30, MIN_WIDTH, 96), 11);
        // The hard cap still applies, and the floor holds against a lone holder.
        assert_eq!(class_demand(11, 8, 30, MIN_WIDTH, 4), 4);
        assert_eq!(class_demand(11, 1, 0, MIN_WIDTH, 96), MIN_WIDTH);
    }

    #[test]
    fn a_class_with_neither_holders_nor_waiters_asks_for_nothing() {
        assert_eq!(class_demand(11, 0, 0, MIN_WIDTH, 96), 0);
        // A single parked waiter is demand.
        assert_eq!(class_demand(11, 0, 1, MIN_WIDTH, 96), MIN_WIDTH);
    }

    #[test]
    fn committed_widths_are_published_untouched_when_demand_fits() {
        let mut widths = [0u32; CLASS_COUNT];
        let mut demands = [0u32; CLASS_COUNT];
        widths[1] = 8;
        demands[1] = 8;
        // A quiet window for class 2: it asks for less than it holds, but with
        // spare budget its committed width is published unchanged.
        widths[2] = 9;
        demands[2] = 3;

        let shares = arbitrate_budget(&widths, &demands, 96, MIN_WIDTH);

        assert_eq!(shares, widths);
    }

    #[test]
    fn oversubscribed_budget_is_scaled_proportionally() {
        let mut demands = [0u32; CLASS_COUNT];
        demands[1] = 30;
        demands[2] = 10;

        let shares = arbitrate_budget(&demands, &demands, 20, 1);

        assert_eq!(shares[1] + shares[2], 20);
        assert!(shares[1] > shares[2], "larger demand keeps a larger share");
        assert_eq!(shares[1], 15);
        assert_eq!(shares[2], 5);
    }

    #[test]
    fn rounding_remainder_goes_to_the_largest_deficit() {
        let mut demands = [0u32; CLASS_COUNT];
        demands[1] = 7;
        demands[2] = 7;
        demands[3] = 7;

        let shares = arbitrate_budget(&demands, &demands, 10, 1);

        assert_eq!(shares[1] + shares[2] + shares[3], 10);
        // floor(7 * 10 / 21) == 3 each, one CPU of remainder to the first
        // class scanned with the largest deficit.
        assert_eq!(shares[1], 4);
        assert_eq!(shares[2], 3);
        assert_eq!(shares[3], 3);
    }

    #[test]
    fn a_scarce_budget_still_seats_the_floor_of_every_demanding_class() {
        let mut demands = [0u32; CLASS_COUNT];
        demands[1] = 100;
        demands[2] = 1;

        let shares = arbitrate_budget(&demands, &demands, 6, MIN_WIDTH);

        // The scaled share of the small class rounds to 0 and is lifted to the
        // floor, which is the one way the total may exceed the budget.
        assert_eq!(shares[2], MIN_WIDTH);
        assert_eq!(shares[1], 5);
        assert_eq!(shares[1] + shares[2], 5 + MIN_WIDTH);
        // Classes the controller never seeded are not classes and stay
        // unlimited.
        assert_eq!(shares[3], 0);
    }

    #[test]
    fn demand_less_classes_neither_reserve_budget_nor_lose_their_width() {
        let mut widths = [0u32; CLASS_COUNT];
        let mut demands = [0u32; CLASS_COUNT];
        // One hot class that can use the whole budget.
        widths[1] = 16;
        demands[1] = 16;
        // A second class with a single waiter.
        widths[2] = 4;
        demands[2] = 2;
        // Twelve committed but idle classes: no holder, no waiter, no demand.
        for class in 3..15 {
            widths[class] = 6;
            demands[class] = 0;
        }

        let shares = arbitrate_budget(&widths, &demands, 16, 1);

        // The budget is split between the two classes that asked for it, and
        // the hot class is not squeezed by the idle ones.
        let demanding: u32 = (1..CLASS_COUNT)
            .filter(|class| demands[*class] > 0)
            .map(|class| shares[class])
            .sum();
        assert_eq!(demanding, 16);
        assert_eq!(shares[1], 15);
        assert_eq!(shares[2], 1);
        // Idle classes keep the width they committed: publishing them narrower
        // would throttle them the moment they wake, and 0 would mean unlimited.
        for (class, share) in shares.iter().enumerate().take(15).skip(3) {
            assert_eq!(*share, 6, "class {class}");
        }
    }

    #[test]
    fn an_idle_class_is_excluded_from_the_budget_sum_it_would_otherwise_fit_in() {
        let mut widths = [0u32; CLASS_COUNT];
        let mut demands = [0u32; CLASS_COUNT];
        widths[1] = 8;
        demands[1] = 8;
        widths[2] = 8;
        demands[2] = 0;

        // Counting the idle class would oversubscribe a budget of 8 and scale
        // the active class down; excluding it leaves the active class whole.
        let shares = arbitrate_budget(&widths, &demands, 8, MIN_WIDTH);

        assert_eq!(shares[1], 8);
        assert_eq!(shares[2], 8);
    }

    #[test]
    fn merge_folds_the_absorbed_class_into_its_canonical_class() {
        let mut classes = [ClassController::new(); CLASS_COUNT];
        classes[2].seed = 4;
        classes[2].width = 4;
        classes[2].cs_ewma_ns = Some(300.0);
        classes[5].seed = 9;
        classes[5].width = 7;
        classes[5].ncs_ewma_ns = Some(3000.0);

        fold_merged_classes(
            &mut classes,
            |class| if class == 5 { 2 } else { class },
            |_| 1_000,
        );

        // Canonical class takes the wider width and the larger seed.
        assert_eq!(classes[2].width, 7);
        assert_eq!(classes[2].seed, 9);
        assert_eq!(classes[2].cs_ewma_ns, Some(300.0));
        assert_eq!(classes[2].ncs_ewma_ns, Some(3000.0));
        // A merge restarts the holdoff and the full-window gate.
        assert_eq!(classes[2].ticks_since_change, 0);
        assert_eq!(classes[2].acquires_at_change, 1_000);
        // The absorbed class asks for nothing, so it publishes 0 (unlimited)
        // and its stale DSQ stays drainable.
        assert!(classes[5].absorbed);
        assert_eq!(classes[5].width, 0);
    }

    #[test]
    fn folding_is_idempotent_across_ticks() {
        let mut classes = [ClassController::new(); CLASS_COUNT];
        classes[2].width = 4;
        classes[5].width = 7;
        let canonical = |class: usize| if class == 5 { 2 } else { class };

        fold_merged_classes(&mut classes, canonical, |_| 0);
        classes[2].width = 5;
        fold_merged_classes(&mut classes, canonical, |_| 0);

        assert_eq!(classes[2].width, 5);
        assert!(classes[5].absorbed);
    }

    #[test]
    fn a_class_enters_the_loop_at_the_floor_whatever_its_first_seed_asked_for() {
        let mut class = ClassController::new();
        class.seed = 9;

        class.commit_initial_width(MIN_WIDTH, 96, 128);

        // The model estimate of the first window buys no width of its own: the
        // class starts at the floor and has to earn the rest one confirmed step
        // at a time.
        assert_eq!(class.width, MIN_WIDTH);
        assert_ne!(class.width, floored_width(class.seed, MIN_WIDTH, 96));
        assert_eq!(class.history(), vec![MIN_WIDTH]);
        // The seed itself is untouched, so it can steer growth from here on.
        assert_eq!(class.seed, 9);
        // Entering the loop starts the holdoff and the full-window gate.
        assert_eq!(class.ticks_since_change, 0);
        assert_eq!(class.acquires_at_change, 128);
    }

    #[test]
    fn a_class_enters_no_wider_than_a_budget_that_cannot_seat_the_floor() {
        let mut class = ClassController::new();
        class.seed = 9;

        class.commit_initial_width(3, 1, 0);

        assert_eq!(class.width, 1);
    }

    #[test]
    fn a_probe_span_never_exceeds_the_ranks_that_exist() {
        assert_eq!(clamped_probe_span(0), 0);
        assert_eq!(clamped_probe_span(2), 2);
        assert_eq!(clamped_probe_span(MANAGED_CLASS_COUNT), MANAGED_CLASS_COUNT);
        assert_eq!(
            clamped_probe_span(MANAGED_CLASS_COUNT + 1),
            MANAGED_CLASS_COUNT
        );
        assert_eq!(clamped_probe_span(u32::MAX), MANAGED_CLASS_COUNT);
        // The probe reads class `rank + 1`, so the widest span it may be given
        // still leaves the highest rank inside the per-class arrays.
        assert!((clamped_probe_span(u32::MAX) as usize) < CLASS_COUNT);
    }

    #[test]
    fn publishing_a_span_through_a_null_pointer_is_a_no_op() {
        store_probe_span(std::ptr::null_mut(), 4);
    }

    #[test]
    fn a_published_span_lands_clamped_in_the_slot() {
        let mut slot = 0u32;

        store_probe_span(&mut slot, 2);
        assert_eq!(slot, 2);

        store_probe_span(&mut slot, u32::MAX);
        assert_eq!(slot, MANAGED_CLASS_COUNT);
    }

    #[test]
    fn width_history_keeps_the_most_recent_values_in_order() {
        let mut class = ClassController::new();
        for width in 1..=10 {
            class.commit_width(width, 0);
        }

        assert_eq!(class.history(), vec![3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(class.changes, 10);
    }
}
